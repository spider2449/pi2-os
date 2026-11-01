//! Docker API endpoints that go beyond create/start/stop: `events`, `stats`,
//! `top`, `cp` (the archive endpoints), image and volume `prune` and
//! `system df`. They are plain methods on `Daemon`; routing lives in
//! `daemon.rs`.

use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use super::*;
use crate::util::base64_encode;

/// Number of past events kept for `docker events --since`.
const EVENT_HISTORY: usize = 256;

/// Fan-out of daemon events to `GET /events` streams.
#[derive(Default)]
pub(super) struct EventBus {
    subs: Vec<mpsc::Sender<Value>>,
    history: VecDeque<Value>,
}

/// Counts running pulls so that layer garbage collection never deletes a
/// layer that was downloaded but is not referenced by an image yet.
pub(super) struct PullGuard<'a>(&'a AtomicUsize);

impl<'a> PullGuard<'a> {
    pub(super) fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for PullGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn done() -> ApiError {
    // Streaming handlers wrote a complete response themselves.
    ApiError(0, String::new())
}

impl Daemon {
    // --------------------------------------------------------------- events

    pub(super) fn emit(&self, kind: &str, action: &str, id: &str, attrs: Value) {
        let ns = now_ns();
        let mut ev = json!({
            "status": action, "id": id, "Type": kind, "Action": action,
            "Actor": {"ID": id, "Attributes": attrs},
            "scope": "local", "time": ns / 1_000_000_000, "timeNano": ns,
        });
        if let Some(image) = ev["Actor"]["Attributes"]["image"].as_str().map(str::to_string) {
            ev["from"] = json!(image);
        }
        let mut bus = self.events.lock().unwrap_or_else(|e| e.into_inner());
        bus.history.push_back(ev.clone());
        while bus.history.len() > EVENT_HISTORY {
            bus.history.pop_front();
        }
        bus.subs.retain(|s| s.send(ev.clone()).is_ok());
    }

    pub(super) fn emit_container(&self, c: &Container, action: &str, extra: &[(&str, String)]) {
        let mut attrs = serde_json::Map::new();
        for (k, v) in c.request.labels.iter().flatten() {
            attrs.insert(k.clone(), json!(v));
        }
        attrs.insert("image".into(), json!(c.image));
        attrs.insert("name".into(), json!(c.name));
        for (k, v) in extra {
            attrs.insert((*k).into(), json!(v));
        }
        self.emit("container", action, &c.id, Value::Object(attrs));
    }

    /// `GET /events`: stream daemon events as JSON lines.
    pub(super) fn events(&self, req: &Request, w: &mut UnixStream) -> ApiResult<()> {
        let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
        let nanos = |k: &str| req.query(k).and_then(|s| s.parse::<f64>().ok()).filter(|s| *s > 0.0).map(|s| (s * 1e9) as i64);
        let (since, until) = (nanos("since"), nanos("until"));
        let wanted = |ev: &Value| -> bool {
            let attr = |k: &str| ev["Actor"]["Attributes"][k].as_str().unwrap_or_default().to_string();
            let (kind, action, id) = (ev["Type"].as_str().unwrap_or_default(), ev["Action"].as_str().unwrap_or_default(), ev["id"].as_str().unwrap_or_default());
            let labels: BTreeMap<String, String> = ev["Actor"]["Attributes"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default();
            f.any("type", |t| t == kind)
                && f.any("event", |e| e == action)
                && f.any("container", |c| kind == "container" && (id.starts_with(c) || attr("name") == c))
                && f.any("image", |i| attr("image") == i || (kind == "image" && id == i))
                && f.any("volume", |v| kind == "volume" && id == v)
                && f.any("network", |n| kind == "network" && (id.starts_with(n) || attr("name") == n))
                && f.labels_match(&labels)
        };
        let (tx, rx) = mpsc::channel();
        let backlog: Vec<Value> = {
            let mut bus = self.events.lock().unwrap_or_else(|e| e.into_inner());
            bus.subs.push(tx);
            match since {
                Some(s) => bus.history.iter().filter(|e| e["timeNano"].as_i64().unwrap_or(0) >= s).cloned().collect(),
                None => Vec::new(),
            }
        };
        http::respond_chunked(w, 200, "application/json")?;
        let send = |w: &mut UnixStream, ev: &Value| write_chunk(w, format!("{ev}\n").as_bytes());
        let in_range = |ev: &Value| until.is_none_or(|u| ev["timeNano"].as_i64().unwrap_or(0) <= u);
        for ev in backlog.iter().filter(|e| wanted(e) && in_range(e)) {
            send(w, ev)?;
        }
        // Without `until` the stream stays open; with a past `until` it ends now.
        loop {
            if until.is_some_and(|u| u <= now_ns()) {
                break;
            }
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(ev) => {
                    if !in_range(&ev) {
                        break;
                    }
                    if wanted(&ev) && send(w, &ev).is_err() {
                        return Err(done());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if peer_closed(w) {
                        return Err(done());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = w.write_all(b"0\r\n\r\n");
        Err(done())
    }

    // ---------------------------------------------------------------- stats

    /// One `GET /containers/{id}/stats` sample, read from the container's cgroup.
    fn stats_sample(&self, ctr: &Ctr, pre: Option<&Value>) -> Value {
        let (id, name, pid, running, host_net) = {
            let c = ctr.lock();
            (c.id.clone(), c.name.clone(), c.state.pid, c.running(), c.net == NetMode::Host)
        };
        let cg = network::cgroup_path(&id);
        let read = |f: &str| fs::read_to_string(cg.join(f)).unwrap_or_default();
        let kv = |text: &str| -> BTreeMap<String, u64> {
            text.lines().filter_map(|l| l.split_once(' ')).filter_map(|(k, v)| Some((k.to_string(), v.trim().parse().ok()?))).collect()
        };
        let cpu = kv(&read("cpu.stat"));
        let mem_stat = kv(&read("memory.stat"));
        let total_ram = pios_core::sys::total_ram();
        let limit = read("memory.max").trim().parse::<u64>().unwrap_or(total_ram).min(total_ram.max(1));
        let ticks: u64 = fs::read_to_string("/proc/stat")
            .unwrap_or_default()
            .lines()
            .next()
            .map(|l| l.split_whitespace().skip(1).filter_map(|v| v.parse::<u64>().ok()).sum())
            .unwrap_or(0);
        let online = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut networks = serde_json::Map::new();
        if running && pid > 0 && !host_net {
            let dev = fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default();
            for l in dev.lines().skip(2) {
                let Some((iface, rest)) = l.split_once(':') else { continue };
                let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if iface.trim() == "lo" || v.len() < 12 {
                    continue;
                }
                networks.insert(
                    iface.trim().to_string(),
                    json!({"rx_bytes": v[0], "rx_packets": v[1], "rx_errors": v[2], "rx_dropped": v[3],
                           "tx_bytes": v[8], "tx_packets": v[9], "tx_errors": v[10], "tx_dropped": v[11]}),
                );
            }
        }
        let mut io_entries = Vec::new();
        for l in read("io.stat").lines() {
            let mut it = l.split_whitespace();
            let Some((major, minor)) = it.next().and_then(|d| d.split_once(':')) else { continue };
            for kv in it {
                let (op, val) = match kv.split_once('=') {
                    Some(("rbytes", v)) => ("read", v),
                    Some(("wbytes", v)) => ("write", v),
                    _ => continue,
                };
                io_entries.push(json!({"major": major.parse::<u64>().unwrap_or(0), "minor": minor.parse::<u64>().unwrap_or(0), "op": op, "value": val.parse::<u64>().unwrap_or(0)}));
            }
        }
        let cpu_stats = json!({
            "cpu_usage": {"total_usage": cpu.get("usage_usec").copied().unwrap_or(0) * 1000,
                          "usage_in_kernelmode": cpu.get("system_usec").copied().unwrap_or(0) * 1000,
                          "usage_in_usermode": cpu.get("user_usec").copied().unwrap_or(0) * 1000},
            "system_cpu_usage": ticks * 10_000_000, "online_cpus": online,
            "throttling_data": {"periods": cpu.get("nr_periods").copied().unwrap_or(0),
                                "throttled_periods": cpu.get("nr_throttled").copied().unwrap_or(0),
                                "throttled_time": cpu.get("throttled_usec").copied().unwrap_or(0) * 1000},
        });
        let now = now_ns();
        json!({
            "read": rfc3339(now), "preread": pre.map_or("0001-01-01T00:00:00Z".to_string(), |p| p["read"].as_str().unwrap_or_default().to_string()),
            "pids_stats": {"current": read("pids.current").trim().parse::<u64>().unwrap_or(0)},
            "num_procs": 0,
            "storage_stats": {},
            "blkio_stats": {"io_service_bytes_recursive": io_entries},
            "cpu_stats": cpu_stats,
            "precpu_stats": pre.map_or(json!({"cpu_usage": {"total_usage": 0}}), |p| p["cpu_stats"].clone()),
            "memory_stats": {"usage": read("memory.current").trim().parse::<u64>().unwrap_or(0), "limit": limit, "stats": mem_stat},
            "name": format!("/{name}"), "id": id, "networks": networks,
        })
    }

    pub(super) fn stats(&self, ctr: &Arc<Ctr>, req: &Request, w: &mut UnixStream) -> ApiResult<Option<(u16, Value)>> {
        let stream = req.query("stream").is_none_or(|v| matches!(v, "1" | "true" | "True"));
        let first = self.stats_sample(ctr, None);
        if !stream {
            // Like dockerd, report CPU over one second unless it is a one-shot call.
            if req.flag("one-shot") || !ctr.lock().running() {
                return Ok(Some((200, first)));
            }
            std::thread::sleep(Duration::from_secs(1));
            return Ok(Some((200, self.stats_sample(ctr, Some(&first)))));
        }
        http::respond_chunked(w, 200, "application/json")?;
        let mut prev = first;
        write_chunk(w, format!("{prev}\n").as_bytes())?;
        while ctr.lock().running() && !ctr.removed.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            let cur = self.stats_sample(ctr, Some(&prev));
            if write_chunk(w, format!("{cur}\n").as_bytes()).is_err() {
                return Err(done());
            }
            prev = cur;
        }
        let _ = w.write_all(b"0\r\n\r\n");
        Err(done())
    }

    // ------------------------------------------------------------------ top

    pub(super) fn top(&self, ctr: &Ctr) -> ApiResult<Value> {
        let (id, running) = {
            let c = ctr.lock();
            (c.id.clone(), c.running())
        };
        if !running {
            return Err(ApiError(409, format!("Container {id} is not running")));
        }
        let btime: i64 = fs::read_to_string("/proc/stat")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
            .unwrap_or(0);
        let mut rows: Vec<(u64, Vec<String>)> = Vec::new();
        for pid in fs::read_to_string(network::cgroup_path(&id).join("cgroup.procs")).unwrap_or_default().lines() {
            let Ok(n) = pid.trim().parse::<u64>() else { continue };
            let Ok(stat) = fs::read_to_string(format!("/proc/{n}/stat")) else { continue };
            let in_ns = fs::read_to_string(format!("/proc/{n}/status")).is_ok_and(|s| s.lines().any(|l| l.starts_with("NSpid:") && l.split_whitespace().count() > 2));
            if !in_ns {
                continue;
            }
            // The command name is parenthesised and may contain spaces.
            let Some((head, tail)) = stat.rsplit_once(')') else { continue };
            let rest: Vec<&str> = tail.split_whitespace().collect();
            let field = |i: usize| rest.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            // `rest` starts at field 3 (state): ppid=1, tty_nr=4, utime=11, stime=12, starttime=19.
            let (ppid, tty, cpu_secs, start) = (field(1), field(4), (field(11) + field(12)) / 100, btime + field(19) / 100);
            let uid = fs::read_to_string(format!("/proc/{n}/status"))
                .unwrap_or_default()
                .lines()
                .find_map(|l| l.strip_prefix("Uid:")?.split_whitespace().next().map(str::to_string))
                .unwrap_or_default();
            let user = fs::read_to_string(format!("/proc/{n}/root/etc/passwd"))
                .unwrap_or_default()
                .lines()
                .find_map(|l| {
                    let f: Vec<&str> = l.split(':').collect();
                    (f.get(2) == Some(&uid.as_str())).then(|| f[0].to_string())
                })
                .unwrap_or(uid);
            let cmd = fs::read(format!("/proc/{n}/cmdline"))
                .map(|b| b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let cmd = if cmd.is_empty() { format!("[{}]", head.split_once('(').map_or("", |(_, c)| c)) } else { cmd };
            let tty = if (tty >> 8) == 136 { format!("pts/{}", tty & 0xff) } else { "?".into() };
            let hhmm = {
                let t = start.rem_euclid(86_400);
                format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
            };
            let time = format!("{:02}:{:02}:{:02}", cpu_secs / 3600, cpu_secs % 3600 / 60, cpu_secs % 60);
            rows.push((n, vec![user, n.to_string(), ppid.to_string(), "0".into(), hhmm, tty, time, cmd]));
        }
        rows.sort_by_key(|(n, _)| *n);
        Ok(json!({
            "Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"],
            "Processes": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
        }))
    }

    // -------------------------------------------------------------- archive

    /// Directory in which the container's files can be reached from the host:
    /// `/proc/<pid>/root` while it runs, a temporary overlay mount otherwise.
    fn container_root(&self, ctr: &Ctr) -> ApiResult<RootGuard> {
        let c = ctr.lock();
        if c.running() && c.state.pid > 0 {
            return Ok(RootGuard { path: format!("/proc/{}/root", c.state.pid).into(), mounted: false });
        }
        let merged = ctr.dir.join("merged");
        fs::create_dir_all(&merged)?;
        let lower: Vec<String> = c.layers.iter().rev().map(|l| self.store.layer_path(l).display().to_string()).collect();
        if lower.is_empty() {
            return Err(ApiError(500, "image has no layers".into()));
        }
        let opts = format!("lowerdir={},upperdir={},workdir={}", lower.join(":"), ctr.dir.join("upper").display(), ctr.dir.join("work").display());
        if opts.len() > 4000 {
            return Err(ApiError(500, format!("image has too many layers ({})", lower.len())));
        }
        let (target, data) = (cstr(&merged.display().to_string()), cstr(&opts));
        // SAFETY: valid C strings for the duration of the call.
        let r = unsafe { libc::mount(c"overlay".as_ptr(), target.as_ptr(), c"overlay".as_ptr(), 0, data.as_ptr().cast()) };
        if r < 0 {
            return Err(ApiError(500, format!("mount container filesystem: {}", io::Error::last_os_error())));
        }
        let guard = RootGuard { path: merged.clone(), mounted: true };
        let mut mounts: Vec<&MountPoint> = c.mounts.iter().filter(|m| m.kind != MountKind::Tmpfs).collect();
        mounts.sort_by_key(|m| m.target.len());
        for m in mounts {
            let src = if m.kind == MountKind::Volume { self.volume_path(&m.source) } else { PathBuf::from(&m.source) };
            if !src.is_dir() {
                continue;
            }
            let Ok(dst) = join_in_root(&merged, &m.target, true) else { continue };
            let _ = fs::create_dir_all(&dst);
            let (s, d) = (cstr(&src.display().to_string()), cstr(&dst.display().to_string()));
            // SAFETY: valid C strings; a failed bind only hides that mount from `cp`.
            unsafe { libc::mount(s.as_ptr(), d.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()) };
        }
        Ok(guard)
    }

    /// `HEAD`, `GET` and `PUT /containers/{id}/archive` (`docker cp`).
    pub(super) fn archive(&self, ctr: &Arc<Ctr>, req: &Request, w: &mut UnixStream) -> ApiResult<Option<(u16, Value)>> {
        let path = req.query("path").unwrap_or_default().to_string();
        if path.is_empty() {
            return Err(bad_request("path cannot be empty"));
        }
        let cid = ctr.lock().id.clone();
        let missing = || ApiError(404, format!("Could not find the file {path} in container {cid}"));
        let root = self.container_root(ctr)?;
        let specifies_dir_contents = path.ends_with("/.");
        let put = req.method == "PUT";
        let target = join_in_root(&root.path, &path, put).map_err(|_| missing())?;

        if put {
            let md = fs::metadata(&target).map_err(|_| missing())?;
            if !md.is_dir() {
                return Err(bad_request("extraction point is not a directory"));
            }
            let mut ar = tar::Archive::new(&req.body[..]);
            ar.set_preserve_permissions(true);
            ar.set_preserve_mtime(true);
            // SAFETY: plain getter.
            ar.set_preserve_ownerships(unsafe { libc::geteuid() } == 0);
            ar.set_overwrite(true);
            // Not `Archive::unpack`/`unpack_in`: they canonicalise paths, which turns
            // `/proc/<pid>/root/...` into the host's own root. Every parent
            // directory is resolved by `join_in_root` instead.
            let base = path.trim_end_matches('/').to_string();
            let extract = |ar: &mut tar::Archive<&[u8]>| -> io::Result<()> {
                for entry in ar.entries()? {
                    let mut entry = entry?;
                    let mut rel = PathBuf::new();
                    for c in entry.path()?.components() {
                        match c {
                            std::path::Component::Normal(p) => rel.push(p),
                            std::path::Component::CurDir | std::path::Component::RootDir => {}
                            _ => return Err(io::Error::other(format!("archive entry escapes the destination: {}", entry.path()?.display()))),
                        }
                    }
                    if entry.header().entry_type().is_hard_link() {
                        return Err(io::Error::other("hard links in archives are not supported"));
                    }
                    let Some(name) = rel.file_name().map(|n| n.to_owned()) else { continue };
                    let parent = join_in_root(&root.path, &format!("{base}/{}", rel.parent().unwrap_or(Path::new("")).display()), true)?;
                    fs::create_dir_all(&parent)?;
                    entry.unpack(parent.join(name))?;
                }
                Ok(())
            };
            extract(&mut ar).map_err(|e| ApiError(500, format!("extract archive: {e}")))?;
            return Ok(Some((200, Value::Null)));
        }

        let md = fs::symlink_metadata(&target).map_err(|_| missing())?;
        let base = if specifies_dir_contents {
            ".".to_string()
        } else {
            Path::new(path.trim_end_matches('/')).file_name().map_or("/".to_string(), |n| n.to_string_lossy().into_owned())
        };
        let link_target = if md.file_type().is_symlink() { fs::read_link(&target).map(|t| t.display().to_string()).unwrap_or_default() } else { String::new() };
        let stat = json!({
            "name": base, "size": md.len(), "mode": go_file_mode(&md),
            "mtime": rfc3339(md.mtime() * 1_000_000_000 + md.mtime_nsec()), "linkTarget": link_target,
        });
        let header = format!("X-Docker-Container-Path-Stat: {}\r\n", base64_encode(stat.to_string().as_bytes()));
        if req.method == "HEAD" {
            write!(w, "HTTP/1.1 200 OK\r\n{API_HEADERS}{header}Content-Length: 0\r\n\r\n")?;
            return Err(done());
        }
        write!(w, "HTTP/1.1 200 OK\r\n{API_HEADERS}Content-Type: application/x-tar\r\n{header}Transfer-Encoding: chunked\r\n\r\n")?;
        let out = io::BufWriter::with_capacity(64 << 10, ChunkSink(w.try_clone()?));
        let mut b = tar::Builder::new(out);
        let built = append_tree(&mut b, &target, &base);
        let finished = built.and_then(|_| b.into_inner()).and_then(|mut o| o.flush());
        if let Err(e) = finished {
            warn!("engine", "archive {path}: {e}");
        }
        let _ = w.write_all(b"0\r\n\r\n");
        Err(done())
    }

    // ---------------------------------------------------------------- prune

    /// Bytes used by the files below `p` (apparent size, symlinks not followed).
    fn usage(p: &Path) -> u64 {
        let Ok(md) = fs::symlink_metadata(p) else { return 0 };
        if !md.is_dir() {
            return md.len();
        }
        fs::read_dir(p).map(|rd| rd.flatten().map(|e| Self::usage(&e.path())).sum()).unwrap_or(0)
    }

    /// Layer directories no image and no container refers to (leftovers of
    /// failed pulls or force removed images). Skipped while a pull runs.
    fn orphan_layers(&self) -> Vec<String> {
        if self.pulls.load(Ordering::SeqCst) > 0 {
            return Vec::new();
        }
        let mut keep: HashSet<String> = self.all().iter().flat_map(|c| c.lock().layers.clone()).collect();
        for id in self.store.image_ids() {
            if let Some(img) = self.store.load(&id) {
                keep.extend(img.meta.layers);
            }
        }
        fs::read_dir(self.store.layers_dir())
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !keep.contains(n)).collect())
            .unwrap_or_default()
    }

    /// `POST /images/prune`: unused images (all of them with `dangling=false`,
    /// otherwise only untagged ones), then orphaned layers.
    pub(super) fn prune_images(&self, req: &Request) -> ApiResult<Value> {
        let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
        let all = f.get("dangling").iter().any(|v| v == "false" || v == "0");
        let until = f.get("until").first().and_then(|u| u.parse::<i64>().ok());
        let used: HashSet<String> = self.all().iter().map(|c| c.lock().image_id.clone()).collect();
        let in_use = |id: &str| used.contains(id);
        let (mut deleted, mut reclaimed) = (Vec::new(), 0u64);
        for id in self.store.image_ids() {
            let Some(img) = self.store.load(&id) else { continue };
            let skip = used.contains(&img.id)
                || (!all && !img.tags.is_empty())
                || until.is_some_and(|u| img.meta.created / 1_000_000_000 >= u)
                || !f.labels_match(&img.run_config().labels.unwrap_or_default());
            if skip {
                continue;
            }
            let sizes: BTreeMap<String, u64> = img.meta.layers.iter().map(|l| (l.clone(), Self::usage(&self.store.layer_path(l)))).collect();
            let Ok(out) = self.store.remove(&img.id, true, &in_use) else { continue };
            for e in &out {
                if let Some(l) = e["Deleted"].as_str().and_then(|d| d.strip_prefix("sha256:")) {
                    reclaimed += sizes.get(l).copied().unwrap_or(0);
                }
                for (key, action) in [("Untagged", "untag"), ("Deleted", "delete")] {
                    if let Some(name) = e[key].as_str() {
                        self.emit("image", action, name, json!({"name": name}));
                    }
                }
            }
            deleted.extend(out);
        }
        for l in self.orphan_layers() {
            let p = self.store.layer_path(&l);
            reclaimed += Self::usage(&p);
            let _ = fs::remove_dir_all(&p);
        }
        Ok(json!({"ImagesDeleted": deleted, "SpaceReclaimed": reclaimed}))
    }

    /// Names of the volumes some container (running or not) mounts.
    fn volumes_in_use(&self) -> HashSet<String> {
        self.all()
            .iter()
            .flat_map(|c| c.lock().mounts.iter().filter(|m| m.kind == MountKind::Volume).map(|m| m.source.clone()).collect::<Vec<_>>())
            .collect()
    }

    /// `POST /volumes/prune`: anonymous volumes, or all unused ones with `all=true`.
    pub(super) fn prune_volumes(&self, req: &Request) -> ApiResult<Value> {
        let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
        let all = f.get("all").iter().any(|v| v == "true" || v == "1");
        let used = self.volumes_in_use();
        let names: Vec<String> = fs::read_dir(self.volumes_dir())
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let (mut deleted, mut reclaimed) = (Vec::new(), 0u64);
        for name in names.into_iter().filter(|n| !used.contains(n)) {
            let anonymous = name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit());
            if !(all || anonymous) || !f.labels_match(&self.volume_labels(&name)) {
                continue;
            }
            let p = self.volumes_dir().join(&name);
            let size = Self::usage(&p);
            if fs::remove_dir_all(&p).is_ok() {
                reclaimed += size;
                self.emit("volume", "destroy", &name, json!({}));
                deleted.push(name);
            }
        }
        Ok(json!({"VolumesDeleted": deleted, "SpaceReclaimed": reclaimed}))
    }

    // ------------------------------------------------------------ system df

    /// `GET /system/df` (`docker system df`).
    pub(super) fn disk_usage(&self) -> Value {
        let used_by: Vec<String> = self.all().iter().map(|c| c.lock().image_id.clone()).collect();
        let mut images: Vec<Value> = self
            .store
            .image_ids()
            .iter()
            .filter_map(|i| self.store.load(i))
            .map(|i| {
                let mut v = self.store.summary(&i);
                v["Containers"] = json!(used_by.iter().filter(|u| **u == i.id).count());
                v
            })
            .collect();
        images.sort_by_key(|v| -v["Created"].as_i64().unwrap_or(0));
        let vols = self.volumes_dir().display().to_string();
        let containers: Vec<Value> = self
            .all()
            .iter()
            .map(|c| {
                let g = c.lock();
                let mut v = g.summary(&self.net_ref(&g).info(), &vols);
                v["SizeRw"] = json!(Self::usage(&c.dir.join("upper")));
                v["SizeRootFs"] = json!(g.layers.iter().map(|l| Self::usage(&self.store.layer_path(l))).sum::<u64>());
                v
            })
            .collect();
        let refs: Vec<String> = self
            .all()
            .iter()
            .flat_map(|c| c.lock().mounts.iter().filter(|m| m.kind == MountKind::Volume).map(|m| m.source.clone()).collect::<Vec<_>>())
            .collect();
        let volumes: Vec<Value> = fs::read_dir(self.volumes_dir())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .map(|n| {
                        let mut v = self.volume_json(&n);
                        v["UsageData"] = json!({"Size": Self::usage(&self.volumes_dir().join(&n)), "RefCount": refs.iter().filter(|r| **r == n).count()});
                        v
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "LayersSize": Self::usage(&self.store.layers_dir()),
            "Images": images, "Containers": containers, "Volumes": volumes, "BuildCache": [],
        })
    }
}

/// Where a container's files are visible from the host; unmounts a temporary
/// overlay mount when dropped.
struct RootGuard {
    path: PathBuf,
    mounted: bool,
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        if self.mounted {
            let p = cstr(&self.path.display().to_string());
            // SAFETY: valid C string.
            unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

/// `Write` adapter that sends everything as HTTP chunks.
struct ChunkSink(UnixStream);

impl Write for ChunkSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_chunk(&mut self.0, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("no NUL bytes")
}

/// The client closed its end of a stream the server only writes to.
fn peer_closed(w: &UnixStream) -> bool {
    let mut p = libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLIN | libc::POLLRDHUP, revents: 0 };
    // SAFETY: one valid pollfd; zero timeout.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLIN | libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

/// Like `runtime::secure_join`, but the final component is followed only with
/// `follow_last` (`docker cp` copies a symlink itself, not its target).
fn join_in_root(root: &Path, path: &str, follow_last: bool) -> io::Result<PathBuf> {
    let mut todo: VecDeque<String> = path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let mut done: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(part) = todo.pop_front() {
        match part.as_str() {
            "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        let candidate = done.iter().fold(root.to_path_buf(), |p, c| p.join(c)).join(&part);
        let is_link = fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && (follow_last || !todo.is_empty()) {
            links += 1;
            if links > 40 {
                return Err(io::Error::other("too many levels of symbolic links"));
            }
            let target = fs::read_link(&candidate)?.display().to_string();
            if target.starts_with('/') {
                done.clear();
            }
            for t in target.split('/').filter(|s| !s.is_empty()).rev() {
                todo.push_front(t.to_string());
            }
        } else {
            done.push(part);
        }
    }
    Ok(done.iter().fold(root.to_path_buf(), |p, c| p.join(c)))
}

/// Add `src` (a file, symlink or directory tree) to the archive as `name`.
/// The name is written into the header as given, so "dir/." yields "./x"
/// entries as dockerd does (`Header::set_path` would strip the "./").
fn append_tree<W: Write>(b: &mut tar::Builder<W>, src: &Path, name: &str) -> io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    let ft = md.file_type();
    if !(ft.is_dir() || ft.is_file() || ft.is_symlink()) {
        return Ok(());
    }
    let mut h = tar::Header::new_gnu();
    h.set_metadata(&md);
    let full = if ft.is_dir() { format!("{name}/") } else { name.to_string() };
    if full.len() <= 100 {
        let field = &mut h.as_old_mut().name;
        field.fill(0);
        field[..full.len()].copy_from_slice(full.as_bytes());
    } else {
        h.set_path(&full)?;
    }
    if ft.is_symlink() {
        h.set_link_name(fs::read_link(src)?)?;
    }
    h.set_cksum();
    if ft.is_file() {
        b.append(&h, fs::File::open(src)?)?;
    } else {
        b.append(&h, io::empty())?;
    }
    if ft.is_dir() {
        for e in fs::read_dir(src)? {
            let e = e?;
            append_tree(b, &e.path(), &format!("{name}/{}", e.file_name().to_string_lossy()))?;
        }
    }
    Ok(())
}

/// Go's `os.FileMode` bits, which the `X-Docker-Container-Path-Stat` header uses.
fn go_file_mode(md: &fs::Metadata) -> u32 {
    let m = md.mode();
    let t = md.file_type();
    let mut out = m & 0o777;
    for (cond, bit) in [
        (t.is_dir(), 1u32 << 31),
        (t.is_symlink(), 1 << 27),
        (t.is_block_device(), 1 << 26),
        (t.is_char_device(), (1 << 26) | (1 << 21)),
        (t.is_fifo(), 1 << 25),
        (t.is_socket(), 1 << 24),
        (m & libc::S_ISUID != 0, 1 << 23),
        (m & libc::S_ISGID != 0, 1 << 22),
        (m & libc::S_ISVTX != 0, 1 << 20),
    ] {
        if cond {
            out |= bit;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_in_root_stays_inside_root() {
        let root = std::env::temp_dir().join(format!("pios-join-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("link")).unwrap();
        std::os::unix::fs::symlink("../../../etc", root.join("up")).unwrap();
        assert_eq!(join_in_root(&root, "/link/hosts", false).unwrap(), root.join("etc/hosts"));
        assert_eq!(join_in_root(&root, "/up/hosts", false).unwrap(), root.join("etc/hosts"));
        assert_eq!(join_in_root(&root, "/../../etc", false).unwrap(), root.join("etc"));
        // The last component stays a link unless asked to follow it.
        assert_eq!(join_in_root(&root, "/link", false).unwrap(), root.join("link"));
        assert_eq!(join_in_root(&root, "/link", true).unwrap(), root.join("etc"));
        let _ = fs::remove_dir_all(&root);
    }
}

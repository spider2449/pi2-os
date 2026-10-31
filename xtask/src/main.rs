//! `cargo xtask <command>` — builds the PiOS SD card image.
//!
//! Every command needs Linux tooling. On any other host (or when not already
//! inside the build container) xtask re-executes itself inside the
//! `pios-build` Docker image, so `cargo xtask all` works from Windows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

const TARGET: &str = "armv7-unknown-linux-musleabihf";
const BUILD_IMAGE: &str = "pios-build";
/// Boot, rootfs A/B and initial data partition sizes (MiB).
const BOOT_MIB: u64 = 128;
const ROOTFS_MIB: u64 = 256;
const DATA_MIB: u64 = 64;

type Result<T = ()> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: cargo xtask <command>

  build        cross-compile the pios binary (release, cortex-a7)
  kernel       fetch, configure and build the Raspberry Pi kernel
  firmware     download pinned GPU firmware
  thirdparty   download docker static binaries and Alpine helper packages
  certs        generate mTLS certificates for remote docker access
  rootfs       assemble and compress the root filesystem (squashfs)
  image        assemble out/images/pios.img
  bundle       pack rootfs, kernel and firmware into out/pios-update.tar
  all          everything above except certs, in order
  qemu         boot the image in qemu-system-arm -M raspi2b
  deploy       rebuild pios + rootfs, then A/B-update a running board over SSH
               (PIOS_HOST, default pios; PIOS_DEPLOY_FIRMWARE=1 also replaces
               the GPU firmware)
  test         run host unit tests
  shell        open a shell in the build container

options: --native  run on this host even if not in the container";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let native = take_flag(&mut args, "--native");
    let Some(cmd) = args.first().cloned() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let root = workspace_root();
    let in_container = std::env::var_os("PIOS_IN_CONTAINER").is_some();
    // A Linux host with the native toolchain (build/setup-wsl.sh) builds
    // natively. Deciding by "is docker installed" is wrong: the CLI may point
    // at a remote engine such as the Pi itself.
    let native = native || (cfg!(target_os = "linux") && (has_native_toolchain() || !has_docker()));
    let r = if cmd == "test" {
        run(Command::new("cargo").current_dir(&root).args(["test", "--workspace", "--exclude", "xtask"]))
    } else if !in_container && !native && cmd != "help" {
        in_build_container(&root, &args)
    } else {
        // PIOS_WORK_DIR moves kernel sources and downloads off slow mounts
        // such as WSL's /mnt/<drive>.
        let work = std::env::var_os("PIOS_WORK_DIR").map(PathBuf::from).unwrap_or(root.join("work"));
        let x = Xtask { root: root.clone(), out: root.join("out"), work };
        x.dispatch(&cmd)
    };
    if let Err(e) = r {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

fn has_native_toolchain() -> bool {
    std::env::var_os("PIOS_NATIVE").is_some()
        || ["apk.static", "cargo-zigbuild", "genimage"].iter().all(|t| {
            Command::new("sh").args(["-c", &format!("command -v {t}")]).output().is_ok_and(|o| o.status.success())
        })
}

fn has_docker() -> bool {
    Command::new("docker").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(cmd: &mut Command) -> Result {
    eprintln!("+ {cmd:?}");
    let status = cmd.status().map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
    status.success().then_some(()).ok_or_else(|| format!("{:?} failed: {status}", cmd.get_program()))
}

fn sh(script: &str) -> Result {
    run(Command::new("bash").args(["-euo", "pipefail", "-c", script]))
}

fn in_build_container(root: &Path, args: &[String]) -> Result {
    run(Command::new("docker").args(["build", "-t", BUILD_IMAGE, "-f"]).arg(root.join("build/Dockerfile")).arg(root.join("build")))?;
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--init"]);
    if args.first().is_some_and(|a| a == "shell" || a == "qemu") {
        cmd.arg("-it");
    }
    // Kernel sources and caches live in named volumes: a Windows bind mount
    // is far too slow for a kernel build. Only `out/` needs to reach the host.
    cmd.arg("-v").arg(format!("{}:/src", root.display()))
        .args(["-v", "pios-work:/src/work", "-v", "pios-target:/src/target", "-v", "pios-cargo:/opt/cargo/registry"])
        .args(["-e", "PIOS_IN_CONTAINER=1", "-w", "/src", BUILD_IMAGE, "cargo", "xtask"])
        .args(args);
    run(&mut cmd)
}

#[derive(Deserialize)]
struct Lock {
    docker: DockerLock,
    alpine: AlpineLock,
}

#[derive(Deserialize)]
struct DockerLock {
    version: String,
    url: String,
    sha256: String,
    binaries: Vec<String>,
}

#[derive(Deserialize)]
struct AlpineLock {
    version: String,
    mirror: String,
    arch: String,
    repositories: Vec<String>,
    packages: Vec<String>,
}

struct Xtask {
    root: PathBuf,
    out: PathBuf,
    work: PathBuf,
}

impl Xtask {
    fn dispatch(&self, cmd: &str) -> Result {
        std::fs::create_dir_all(&self.out).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.work).map_err(|e| e.to_string())?;
        match cmd {
            "build" => self.build(),
            "kernel" => self.kernel(),
            "firmware" => self.firmware(),
            "thirdparty" => self.thirdparty(),
            "certs" => self.certs(),
            "rootfs" => self.rootfs(),
            "image" => self.image(),
            "all" => {
                self.build()?;
                self.kernel()?;
                self.firmware()?;
                self.thirdparty()?;
                self.rootfs()?;
                self.image()?;
                self.bundle()
            }
            "qemu" => self.qemu(),
            "bundle" => self.bundle(),
            "deploy" => self.deploy(),
            "shell" => run(&mut Command::new("bash")),
            _ => Err(format!("unknown command '{cmd}'\n{USAGE}")),
        }
    }

    fn p(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }

    fn lock(&self) -> Result<Lock> {
        let text = std::fs::read_to_string(self.root.join("third_party.lock")).map_err(|e| e.to_string())?;
        toml::from_str(&text).map_err(|e| format!("third_party.lock: {e}"))
    }

    fn kv(&self, rel: &str) -> Result<std::collections::HashMap<String, String>> {
        let text = std::fs::read_to_string(self.root.join(rel)).map_err(|e| format!("{rel}: {e}"))?;
        Ok(text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect())
    }

    /// Download `url` to `dest` unless it already exists with the right hash.
    fn fetch(&self, url: &str, sha256: &str, dest: &Path) -> Result {
        let d = dest.display();
        sh(&format!(
            "if ! echo '{sha256}  {d}' | sha256sum -c --status 2>/dev/null; then \
               curl -fsSL --retry 3 -o '{d}.part' '{url}' && \
               echo '{sha256}  {d}.part' | sha256sum -c --quiet && mv '{d}.part' '{d}'; fi"
        ))
    }

    fn build(&self) -> Result {
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        // mimalloc-sys links -latomic on ARM, which zig does not ship. ARMv7
        // has native atomics (ldrex/strex), so an empty archive satisfies it.
        let shim = self.work.join("zig-shim");
        sh(&format!("mkdir -p '{d}' && [ -f '{d}/libatomic.a' ] || ar rc '{d}/libatomic.a'", d = shim.display()))?;
        let flags = format!("target.{TARGET}.rustflags=[\"-L\",\"native={}\"]", shim.display());
        run(Command::new("cargo")
            .current_dir(&self.root)
            .env("PIOS_BUILD_EPOCH", epoch.to_string())
            // --config must follow the subcommand to reach cargo-zigbuild.
            .args(["zigbuild", "--config", &flags])
            .args(["--release", "--target", TARGET, "-p", "pios", "--features", "mimalloc"]))
    }

    fn pios_binary(&self) -> PathBuf {
        let target_dir = std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or(self.root.join("target"));
        target_dir.join(TARGET).join("release/pios")
    }

    fn kernel(&self) -> Result {
        let v = self.kv("kernel/VERSION")?;
        let src = self.work.join("linux");
        let out = self.out.join("kernel");
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        sh(&format!(
            r#"
            src='{src}'; commit='{commit}'
            if [ ! -d "$src/.git" ]; then git init -q "$src"; git -C "$src" remote add origin '{repo}'; fi
            if [ "$(git -C "$src" rev-parse HEAD 2>/dev/null)" != "$commit" ]; then
              git -C "$src" fetch --depth 1 origin "$commit"
              git -C "$src" checkout -q --force FETCH_HEAD
            fi
            cd "$src"
            export ARCH=arm CROSS_COMPILE=arm-linux-gnueabihf- KBUILD_BUILD_USER=pios KBUILD_BUILD_HOST=pios
            make -s {defconfig}
            ./scripts/kconfig/merge_config.sh -m -O . .config '{fragment}'
            make -s olddefconfig
            make -s -j"$(nproc)" zImage dtbs
            cp arch/arm/boot/zImage '{out}/kernel7.img'
            cp arch/arm/boot/dts/broadcom/bcm2709-rpi-2-b.dtb '{out}/'
            cp .config '{out}/config'
            # Report Docker requirements that the fragment failed to enable.
            curl -fsSL https://raw.githubusercontent.com/moby/moby/master/contrib/check-config.sh -o '{work}/check-config.sh' || true
            [ -f '{work}/check-config.sh' ] && bash '{work}/check-config.sh' .config | grep -E 'missing' || true
            "#,
            src = src.display(),
            commit = v["commit"],
            repo = v["repo"],
            defconfig = v["defconfig"],
            fragment = self.p("kernel/pios.config"),
            out = out.display(),
            work = self.work.display(),
        ))
    }

    fn firmware(&self) -> Result {
        let text = std::fs::read_to_string(self.root.join("board/rpi2/firmware.lock")).map_err(|e| e.to_string())?;
        let mut commit = None;
        let dir = self.out.join("firmware");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let (name, value) = line.split_once(' ').ok_or("bad firmware.lock line")?;
            if name == "commit" {
                commit = Some(value.trim().to_string());
                continue;
            }
            let c = commit.as_ref().ok_or("firmware.lock: commit must come first")?;
            let url = format!("https://raw.githubusercontent.com/raspberrypi/firmware/{c}/boot/{name}");
            self.fetch(&url, value.trim(), &dir.join(name))?;
        }
        Ok(())
    }

    fn thirdparty(&self) -> Result {
        let lock = self.lock()?;
        let dl = self.work.join("dl");
        std::fs::create_dir_all(&dl).map_err(|e| e.to_string())?;
        let tgz = dl.join(format!("docker-{}.tgz", lock.docker.version));
        self.fetch(&lock.docker.url, &lock.docker.sha256, &tgz)?;
        let dest = self.work.join("docker");
        sh(&format!("rm -rf '{d}' && mkdir -p '{d}' && tar -xzf '{t}' -C '{d}' --strip-components=1", d = dest.display(), t = tgz.display()))?;

        let a = &lock.alpine;
        let apkroot = self.work.join("alpine-root");
        sh(&format!(
            "rm -rf '{r}' && mkdir -p '{r}' && \
             um=; [ \"$(id -u)\" = 0 ] || um=--usermode; \
             apk.static $um --root '{r}' --arch {arch} --initdb --no-scripts --no-cache --no-interactive \
               --keys-dir /opt/apk-keys/{arch} {repos} add {pkgs} && \
             rm -rf '{r}/lib/apk' '{r}/etc/apk' '{r}/var' '{r}/usr/share/man' '{r}/usr/share/doc'",
            r = apkroot.display(),
            arch = a.arch,
            repos = a.repositories.iter().map(|r| format!("-X {}/{}/{r}", a.mirror, a.version)).collect::<Vec<_>>().join(" "),
            pkgs = a.packages.join(" "),
        ))
    }

    fn certs(&self) -> Result {
        let dir = self.out.join("certs");
        sh(&format!(
            r#"
            d='{d}'; mkdir -p "$d/server" "$d/client"; cd "$d"
            # Ed25519 everywhere: an RSA-2048 handshake costs dockerd ~230 ms of
            # CPU on the Cortex-A7 (Go has no 32-bit ARM RSA assembly).
            # An older RSA CA is replaced, which invalidates its old certs.
            if [ -f ca-key.pem ] && ! openssl pkey -in ca-key.pem -noout -text | grep -q ED25519; then
              echo "replacing RSA CA with Ed25519"; rm -f ca-key.pem ca.pem ca.srl
            fi
            [ -f ca-key.pem ] || openssl genpkey -algorithm ed25519 -out ca-key.pem
            [ -f ca.pem ] || openssl req -new -x509 -days 3650 -key ca-key.pem -subj "/CN=pios-ca" -out ca.pem
            openssl genpkey -algorithm ed25519 -out server/server-key.pem
            openssl req -new -key server/server-key.pem -subj "/CN=pios" -out server.csr
            printf 'subjectAltName=DNS:pios,DNS:pios.local,DNS:localhost,IP:127.0.0.1%s\nextendedKeyUsage=serverAuth\n' "${{PIOS_CERT_SAN:+,$PIOS_CERT_SAN}}" > server.ext
            openssl x509 -req -days 3650 -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out server/server-cert.pem -extfile server.ext
            openssl genpkey -algorithm ed25519 -out client/key.pem
            openssl req -new -key client/key.pem -subj "/CN=client" -out client.csr
            echo extendedKeyUsage=clientAuth > client.ext
            openssl x509 -req -days 3650 -in client.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out client/cert.pem -extfile client.ext
            cp ca.pem server/ca.pem; cp ca.pem client/ca.pem
            rm -f *.csr *.ext
            echo "server certs: $d/server (copied to the boot partition by 'image')"
            echo "client certs: $d/client (use: docker context create pi --docker host=tcp://<ip>:2376,ca=ca.pem,cert=cert.pem,key=key.pem)"
            "#,
            d = dir.display()
        ))
    }

    fn rootfs(&self) -> Result {
        let lock = self.lock()?;
        let r = self.work.join("rootfs");
        let bin = self.pios_binary();
        if !bin.exists() {
            return Err(format!("{} missing; run 'cargo xtask build'", bin.display()));
        }
        let docker_bins: Vec<String> = lock.docker.binaries.iter().map(|b| format!("'{}/docker/{b}'", self.work.display())).collect();
        sh(&format!(
            r#"
            r='{r}'; rm -rf "$r"; mkdir -p "$r"
            cp -a '{alpine}/.' "$r/"
            cd "$r"
            mkdir -p boot data dev proc sys run tmp var etc sbin usr/bin usr/sbin
            ln -sfn /run/pios/root root   # root home lives on tmpfs (read-only rootfs)
            install -m 0755 '{bin}' sbin/pios
            ln -sf pios sbin/init; ln -sf pios sbin/pios-ctl; ln -sf ../../sbin/pios usr/bin/pios-ctl
            install -m 0755 {docker_bins} usr/bin/
            # busybox applet links (apk ran without scripts, so its trigger never
            # created them). Real binaries from other packages take precedence.
            while read -r p; do
              [ -e "$p" ] || [ -L "$p" ] || {{ mkdir -p "$(dirname "$p")"; ln -s /bin/busybox "$p"; }}
            done < '{applets}'
            cp -a '{overlay}/.' "$r/"
            ln -sf /run/pios/resolv.conf etc/resolv.conf
            ln -sf /proc/self/mounts etc/mtab
            ln -sf /run/pios/shadow etc/shadow   # written by init from [ssh] password_hash
            printf 'root:x:0:0:root:/root:/bin/sh\n' > etc/passwd
            printf '/bin/sh\n' > etc/shells
            printf 'root:x:0:\ndocker:x:999:\n' > etc/group
            printf 'NAME=PiOS\nID=pios\nVERSION_ID=%s\nPRETTY_NAME="PiOS %s (Docker %s)"\n' '{ver}' '{ver}' '{dver}' > etc/os-release
            mksquashfs "$r" '{out}/rootfs.squashfs' -noappend -all-root -comp lz4 -Xhc -b 256K -quiet
            ls -lh '{out}/rootfs.squashfs'
            "#,
            r = r.display(),
            alpine = self.work.join("alpine-root").display(),
            bin = bin.display(),
            docker_bins = docker_bins.join(" "),
            overlay = self.p("rootfs/overlay"),
            applets = self.p("rootfs/busybox-applets.txt"),
            ver = env!("CARGO_PKG_VERSION"),
            dver = lock.docker.version,
            out = self.out.display(),
        ))?;
        let size = std::fs::metadata(self.out.join("rootfs.squashfs")).map_err(|e| e.to_string())?.len();
        if size > ROOTFS_MIB << 20 {
            return Err(format!("rootfs is {} MiB, partition holds {ROOTFS_MIB} MiB", size >> 20));
        }
        Ok(())
    }

    /// `version` line of update manifests: crate version, date, git commit.
    fn build_version(&self) -> String {
        let commit = Command::new("git")
            .current_dir(&self.root)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map_or("unknown".into(), |o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let date = Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%MZ"])
            .output()
            .map_or(String::new(), |o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        format!("{} {date} {commit}", env!("CARGO_PKG_VERSION"))
    }

    /// Manifest over `files` (name, path), sha256 computed here.
    fn manifest(&self, files: &[(&str, PathBuf)]) -> Result<pios_update::Manifest> {
        let mut m = pios_update::Manifest { version: self.build_version(), files: Vec::new() };
        for (name, path) in files {
            let out = Command::new("sha256sum").arg(path).output().map_err(|e| format!("sha256sum: {e}"))?;
            let text = String::from_utf8_lossy(&out.stdout);
            let sum = text.split_whitespace().next().filter(|_| out.status.success()).ok_or(format!("sha256sum {}", path.display()))?;
            m.files.push((name.to_string(), sum.to_string()));
        }
        Ok(m)
    }

    fn kernel_files(&self) -> Vec<(&'static str, PathBuf)> {
        pios_update::KERNEL_FILES.iter().map(|f| (*f, self.out.join("kernel").join(f))).collect()
    }

    fn image(&self) -> Result {
        let images = self.out.join("images");
        let boot = self.work.join("bootfs");
        // Slot A: kernel, device tree and command line live in p2/ (the
        // firmware's os_prefix), next to the rootfs in partition 2. The flat
        // cmdline.txt mirrors the default slot's for the firmware's fallback.
        let board_config = std::fs::read_to_string(self.root.join("board/rpi2/config.txt")).map_err(|e| e.to_string())?;
        let slot = "p2";
        let manifest = self.manifest(&self.kernel_files())?;
        std::fs::create_dir_all(&self.work).map_err(|e| e.to_string())?;
        let gen = self.work.join("bootgen");
        std::fs::create_dir_all(&gen).map_err(|e| e.to_string())?;
        std::fs::write(gen.join("config.txt"), pios_update::slot_config(&board_config, Some(slot), false)).map_err(|e| e.to_string())?;
        std::fs::write(gen.join("manifest.txt"), manifest.render()).map_err(|e| e.to_string())?;
        sh(&format!(
            r#"
            b='{boot}'; i='{images}'; rm -rf "$b"; mkdir -p "$b/{slot}" "$i"
            cp '{out}'/firmware/* "$b/"
            cp '{out}/kernel/kernel7.img' '{out}/kernel/bcm2709-rpi-2-b.dtb' "$b/{slot}/"
            cp '{gen}/config.txt' "$b/config.txt"
            cp '{gen}/manifest.txt' "$b/{slot}/manifest.txt"
            cp '{board}/cmdline.txt' "$b/cmdline.txt"; cp '{board}/cmdline.txt' "$b/{slot}/cmdline.txt"
            # Board-specific settings (gitignored) win over the shipped example.
            if [ -f '{board}/pios.local.toml' ]; then cp '{board}/pios.local.toml' "$b/pios.toml"
            else cp '{overlay}/etc/pios/pios.toml.example' "$b/pios.toml"; fi
            # SSH public keys (gitignored), used when [ssh] is enabled.
            if [ -f '{board}/authorized_keys' ]; then cp '{board}/authorized_keys' "$b/authorized_keys"; fi
            if [ -d '{out}/certs/server' ]; then mkdir -p "$b/docker-tls"; cp '{out}'/certs/server/*.pem "$b/docker-tls/"; fi
            rm -f "$i/boot.vfat"; mkfs.vfat -F 32 -n PIOS-BOOT -C "$i/boot.vfat" $(({boot_mib} * 1024)) >/dev/null
            mcopy -s -i "$i/boot.vfat" "$b"/* ::/
            rm -f "$i/data.ext4"; mkfs.ext4 -q -L pios-data -E lazy_itable_init=1 "$i/data.ext4" {data_mib}M
            cp '{out}/rootfs.squashfs' "$i/rootfs.squashfs"
            rm -rf '{work}/genimage-tmp'
            genimage --config '{board}/genimage.cfg' --inputpath "$i" --outputpath "$i" --tmppath '{work}/genimage-tmp' --rootpath "$b"
            ls -lh "$i/pios.img"
            "#,
            boot = boot.display(),
            images = images.display(),
            gen = gen.display(),
            out = self.out.display(),
            board = self.p("board/rpi2"),
            overlay = self.p("rootfs/overlay"),
            boot_mib = BOOT_MIB,
            data_mib = DATA_MIB,
            work = self.work.display(),
        ))
    }

    /// out/pios-update.tar: manifest, kernel, firmware and rootfs, in the
    /// order `pios update apply` consumes them (rootfs last, streamed).
    fn bundle(&self) -> Result {
        let rootfs = self.out.join("rootfs.squashfs");
        if !rootfs.exists() {
            return Err(format!("{} missing; run 'cargo xtask rootfs'", rootfs.display()));
        }
        let mut files = self.kernel_files();
        files.extend(pios_update::FIRMWARE_FILES.iter().map(|f| (*f, self.out.join("firmware").join(f))));
        files.push((pios_update::BUNDLE_ROOTFS, rootfs));
        if let Some((_, missing)) = files.iter().find(|(_, p)| !p.exists()) {
            return Err(format!("{} missing; run 'cargo xtask all'", missing.display()));
        }
        let manifest = self.manifest(&files)?;
        let stage = self.work.join("bundle");
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        std::fs::write(stage.join(pios_update::MANIFEST), manifest.render()).map_err(|e| e.to_string())?;
        let mut names = vec![pios_update::MANIFEST.to_string()];
        let mut links = String::new();
        for (name, path) in &files {
            links.push_str(&format!("ln -s '{}' '{}/{name}'\n", path.display(), stage.display()));
            names.push(name.to_string());
        }
        sh(&format!(
            "{links}tar --format=ustar --dereference --owner=0 --group=0 -cf '{out}/pios-update.tar' -C '{stage}' {names} && ls -lh '{out}/pios-update.tar'",
            out = self.out.display(),
            stage = stage.display(),
            names = names.join(" "),
        ))?;
        eprintln!("bundle version: {}", manifest.version);
        Ok(())
    }

    /// Stream an update bundle (rootfs + kernel) to a running board, which
    /// writes it to its inactive slot and reboots into it once (firmware
    /// tryboot). The board commits the update when its engine comes up, or
    /// rolls back. A board on a release without bundle support gets the
    /// rootfs alone; deploying again then brings the kernel.
    ///
    /// The pios binary and rootfs are always rebuilt first, so a stale
    /// out/rootfs.squashfs is never shipped under the current version. The
    /// kernel and firmware are taken from out/ as they are (slow to build,
    /// rarely changed; run `cargo xtask kernel` when they do).
    fn deploy(&self) -> Result {
        let host = std::env::var("PIOS_HOST").unwrap_or_else(|_| "pios".into());
        self.build()?;
        self.rootfs()?;
        self.bundle()?;
        let firmware = if std::env::var_os("PIOS_DEPLOY_FIRMWARE").is_some() { "--firmware" } else { "" };
        sh(&format!(
            r#"
            bundle='{bundle}'; rootfs='{rootfs}'; host='{host}'
            st=$(ssh "$host" pios update status); echo "$st"
            case "$st" in
              *"bundles : supported"*) img=$bundle; what="update bundle" ;;
              *) img=$rootfs; what="rootfs (this board predates bundles; deploy again for the kernel)" ;;
            esac
            sum=$(sha256sum "$img" | cut -d' ' -f1)
            echo "deploying $(du -h "$img" | cut -f1) $what (sha256 ${{sum:0:16}}...) to $host"
            # 255 = the connection dropped because the board started rebooting.
            rc=0; ssh "$host" pios update apply --sha256 "$sum" {firmware} --reboot - < "$img" || rc=$?
            [ $rc -eq 0 ] || [ $rc -eq 255 ] || exit $rc
            echo "waiting for $host to come back from the trial boot..."
            sleep 20; start=$(date +%s)
            until ssh -o ConnectTimeout=5 -o BatchMode=yes "$host" true 2>/dev/null; do
              sleep 5; [ $(( $(date +%s) - start )) -lt 900 ] || {{ echo "board unreachable after 15 min"; exit 1; }}
            done
            # The trial commits once the engine is ready (or rolls back).
            for _ in $(seq 180); do
              s=$(ssh "$host" pios update status)
              case "$s" in *"not committed"*) sleep 5 ;; *) break ;; esac
            done
            echo "$s"
            "#,
            bundle = self.out.join("pios-update.tar").display(),
            rootfs = self.out.join("rootfs.squashfs").display(),
        ))
    }

    fn qemu(&self) -> Result {
        let images = self.out.join("images");
        let cmdline = std::fs::read_to_string(self.root.join("board/rpi2/cmdline.txt")).map_err(|e| e.to_string())?;
        // QEMU raspi2b: no smsc95xx (netd picks the usb-net device) and the
        // dwc_otg FIQ fast path hangs the emulated CPUs, so disable it here only.
        // Drop the HDMI console so /dev/console (init output) is the terminal.
        let cmdline = cmdline.trim().replace("console=serial0,115200", "console=ttyAMA0,115200").replace(" console=tty1", "")
            + " dwc_otg.fiq_enable=0 dwc_otg.fiq_fsm_enable=0";
        sh(&format!(
            r#"
            i='{images}'
            cp "$i/pios.img" "$i/pios-qemu.img"
            truncate -s 2G "$i/pios-qemu.img"   # SD size must be a power of two
            # QEMU user networking cannot use a board-specific static IP.
            mcopy -o -i "$i/pios-qemu.img@@4M" '{example}' ::pios.toml
            exec qemu-system-arm -M raspi2b -m 1G -nographic \
              -kernel '{out}/kernel/kernel7.img' -dtb '{out}/kernel/bcm2709-rpi-2-b.dtb' \
              -drive file="$i/pios-qemu.img",if=sd,format=raw \
              -append '{cmdline}' \
              -usb -device usb-net,netdev=n0 \
              -netdev user,id=n0,hostfwd=tcp::2376-:2376,hostfwd=tcp::2377-:2377
            "#,
            images = images.display(),
            out = self.out.display(),
            example = self.p("rootfs/overlay/etc/pios/pios.toml.example"),
        ))
    }
}

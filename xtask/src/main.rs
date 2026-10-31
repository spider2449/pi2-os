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
  all          everything above except certs, in order
  qemu         boot the image in qemu-system-arm -M raspi2b
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
    // A Linux host without docker (e.g. WSL set up by build/setup-wsl.sh)
    // builds natively.
    let native = native || (cfg!(target_os = "linux") && !has_docker());
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
                self.image()
            }
            "qemu" => self.qemu(),
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
        run(Command::new("cargo")
            .current_dir(&self.root)
            .env("PIOS_BUILD_EPOCH", epoch.to_string())
            .args(["zigbuild", "--release", "--target", TARGET, "-p", "pios", "--features", "mimalloc"]))
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
             fr=; [ \"$(id -u)\" = 0 ] || fr=fakeroot; \
             $fr apk.static --root '{r}' --arch {arch} --initdb --no-scripts --no-cache --no-interactive \
               --keys-dir /opt/apk-keys/{arch} -X {mirror}/{ver}/main add {pkgs} && \
             rm -rf '{r}/lib/apk' '{r}/etc/apk' '{r}/var' '{r}/usr/share/man' '{r}/usr/share/doc'",
            r = apkroot.display(),
            arch = a.arch,
            mirror = a.mirror,
            ver = a.version,
            pkgs = a.packages.join(" "),
        ))
    }

    fn certs(&self) -> Result {
        let dir = self.out.join("certs");
        sh(&format!(
            r#"
            d='{d}'; mkdir -p "$d/server" "$d/client"; cd "$d"
            [ -f ca-key.pem ] || openssl genrsa -out ca-key.pem 4096
            [ -f ca.pem ] || openssl req -new -x509 -days 3650 -key ca-key.pem -sha256 -subj "/CN=pios-ca" -out ca.pem
            openssl genrsa -out server/server-key.pem 2048
            openssl req -new -key server/server-key.pem -subj "/CN=pios" -out server.csr
            printf 'subjectAltName=DNS:pios,DNS:pios.local,DNS:localhost,IP:127.0.0.1%s\nextendedKeyUsage=serverAuth\n' "${{PIOS_CERT_SAN:+,$PIOS_CERT_SAN}}" > server.ext
            openssl x509 -req -days 3650 -sha256 -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out server/server-cert.pem -extfile server.ext
            openssl genrsa -out client/key.pem 2048
            openssl req -new -key client/key.pem -subj "/CN=client" -out client.csr
            echo extendedKeyUsage=clientAuth > client.ext
            openssl x509 -req -days 3650 -sha256 -in client.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out client/cert.pem -extfile client.ext
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
            mkdir -p boot data dev proc sys run tmp var etc sbin usr/bin usr/sbin root
            install -m 0755 '{bin}' sbin/pios
            ln -sf pios sbin/init; ln -sf pios sbin/pios-ctl; ln -sf ../../sbin/pios usr/bin/pios-ctl
            install -m 0755 {docker_bins} usr/bin/
            cp -a '{overlay}/.' "$r/"
            ln -sf /run/pios/resolv.conf etc/resolv.conf
            ln -sf /proc/self/mounts etc/mtab
            printf 'root:x:0:0:root:/root:/sbin/nologin\n' > etc/passwd
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

    fn image(&self) -> Result {
        let images = self.out.join("images");
        let boot = self.work.join("bootfs");
        sh(&format!(
            r#"
            b='{boot}'; i='{images}'; rm -rf "$b"; mkdir -p "$b" "$i"
            cp '{out}'/firmware/* "$b/"
            cp '{out}/kernel/kernel7.img' '{out}/kernel/bcm2709-rpi-2-b.dtb' "$b/"
            cp '{board}/config.txt' '{board}/cmdline.txt' "$b/"
            cp '{overlay}/etc/pios/pios.toml.example' "$b/pios.toml"
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
            out = self.out.display(),
            board = self.p("board/rpi2"),
            overlay = self.p("rootfs/overlay"),
            boot_mib = BOOT_MIB,
            data_mib = DATA_MIB,
            work = self.work.display(),
        ))
    }

    fn qemu(&self) -> Result {
        let images = self.out.join("images");
        let cmdline = std::fs::read_to_string(self.root.join("board/rpi2/cmdline.txt")).map_err(|e| e.to_string())?;
        // QEMU's raspi2b has no USB Ethernet chip; netd picks the usb-net device instead.
        let cmdline = cmdline.trim().replace("console=serial0,115200", "console=ttyAMA0,115200");
        sh(&format!(
            r#"
            i='{images}'
            cp "$i/pios.img" "$i/pios-qemu.img"
            truncate -s 2G "$i/pios-qemu.img"   # SD size must be a power of two
            exec qemu-system-arm -M raspi2b -m 1G -nographic \
              -kernel '{out}/kernel/kernel7.img' -dtb '{out}/kernel/bcm2709-rpi-2-b.dtb' \
              -drive file="$i/pios-qemu.img",if=sd,format=raw \
              -append '{cmdline}' \
              -usb -device usb-net,netdev=n0 -netdev user,id=n0,hostfwd=tcp::2376-:2376
            "#,
            images = images.display(),
            out = self.out.display(),
        ))
    }
}

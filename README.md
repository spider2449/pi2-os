# PiOS

專為 **Raspberry Pi 2 Model B v1.1**（BCM2836、4× Cortex-A7、armv7、1 GB RAM）打造、只為跑 Docker 而存在的精簡作業系統。底層用 Linux kernel，**kernel 以上的 userspace 全部是 Rust**。

```
user containers (arm32v7)
dockerd ─ containerd ─ shim ─ runc        ← engine module（可插拔）
pios（單一 multi-call binary）: init(PID 1) · supervisor · netd · ntpd · storage · tune · ctl
Linux 6.18 (rpi, bcm2709)：驅動 built-in、無 initramfs
GPU firmware (start_cd.elf)
```

## 模組（`crates/`）

| crate | 職責 |
|---|---|
| `pios-core` | 設定檔 schema（`/boot/pios.toml`）、服務模型、IPC 協定、logging、syscall 包裝 |
| `pios-init` | PID 1：早期掛載、開機流程、交給 supervisor、乾淨關機 |
| `pios-supervisor` | 依賴排序、重啟退避、每個服務一個 cgroup v2、readiness probe、控制 socket |
| `pios-net` | loopback/uplink、DHCPv4 client（AF_PACKET）、靜態 IP、resolv.conf、SNTP |
| `pios-storage` | 首次開機擴張 data 分割區（MBR + BLKPG）、fsck、掛載、zram swap |
| `pios-tune` | sysctl、CPU governor、SD 卡 I/O scheduler |
| `pios-engine-api` | `ContainerEngine` trait —— init 只認得這個介面 |
| `pios-engine-docker` | daemon.json 生成、containerd/dockerd 服務、關機時優雅停止容器 |
| `pios-engine-native` | Rust-native 引擎：單一 daemon 提供 Docker Engine API 子集、自行 pull image、內建 runtime（namespaces、overlayfs、cgroup v2），取代 dockerd + containerd + shim + runc |
| `pios-ctl` | `pios-ctl status / mem / logs / restart / reboot / tryboot`；`ctld` 以 mTLS 對外提供同一套協定 |
| `pios-update` | A/B 更新（rootfs + kernel）：寫入閒置 slot、firmware tryboot、引擎就緒後 commit，失敗自動回滾 |
| `pios` | multi-call binary，以 cargo feature 選擇要編入哪些引擎 |

所有模組在原始碼層獨立、可單獨測試；執行時合併成一個 binary，程式碼只在 page cache 中存在一份。

## 建置（Windows / macOS / Linux）

需求：Docker Desktop（或任何 Docker engine）與 Rust。

```sh
cargo xtask all        # 交叉編譯 pios、kernel、下載 firmware/docker、組 rootfs、產生映像檔
cargo xtask certs      # （選用）產生遠端 docker 用的 mTLS 憑證，之後再跑一次 `cargo xtask image`
cargo xtask test       # host 上跑單元測試
cargo xtask qemu       # 在 qemu-system-arm -M raspi2b 中開機
```

`xtask` 若發現自己不在建置容器內，會自動 `docker build` `build/Dockerfile` 並在容器內重新執行。kernel 原始碼與 cargo 快取放在具名 volume（`pios-work`、`pios-target`、`pios-cargo`），只有 `out/` 會寫回 host。

產出：`out/images/pios.img` —— 用 Raspberry Pi Imager 或 `dd` 燒錄到 microSD。

## SD 卡配置

| 分割區 | 檔案系統 | 大小 | 內容 |
|---|---|---|---|
| p1 | FAT32 | 128 MiB | firmware、`config.txt`、`pios.toml`、`docker-tls/`；`p2/`、`p3/` 各放一個 slot 的 kernel、device tree、`cmdline.txt` |
| p2 | squashfs (lz4) | 256 MiB | 唯讀 rootfs A |
| p3 | squashfs | 256 MiB | 唯讀 rootfs B（A/B 更新） |
| p4 | ext4 | 首次開機擴張至整張卡 | `/var/lib/docker`、`/var/lib/containerd` |

root 唯讀，斷電不會損壞系統；所有寫入都集中在 p4。

## 使用

1. 個人設定放在 `board/rpi2/pios.local.toml`（gitignored，格式見 `rootfs/overlay/etc/pios/pios.toml.example`），SSH 公鑰放在 `board/rpi2/authorized_keys`；`cargo xtask image` 會把它們放進 boot 分割區。
2. 燒錄、接網路線與 HDMI 開機。畫面出現 `pios: engine ready`、ACT 燈變心跳閃，代表 Docker 已可使用。
3. 從 PC 連線（需先 `cargo xtask certs`，憑證名稱為 `pios`，請讓 `pios` 解析到 Pi 的 IP）：

   ```sh
   docker context create pi --docker "host=tcp://pios:2376,ca=out/certs/client/ca.pem,cert=out/certs/client/cert.pem,key=out/certs/client/key.pem"
   docker --context pi run --rm hello-world
   ```

## 管理管道

| 管道 | 埠 | 說明 |
|---|---|---|
| Docker API | 2376 | mTLS（Ed25519） |
| `tools/pios-remote` | 2377 | `status` / `logs` / `restart` / `reboot`；經 `ctld` 轉給 PID 1，**dockerd 掛掉也能用** |
| SSH（dropbear，選用） | 22 | 只允許金鑰登入；`[ssh] enabled = true` |
| HDMI console / `pios-boot.log` / ACT LED | — | 開機與錯誤診斷，不需要網路 |

### 系統更新（A/B，不需重燒 SD 卡）

```sh
cargo xtask rootfs && cargo xtask deploy     # PIOS_HOST 預設為 pios
```

每個 slot 是一個 root 分割區加上 boot 分割區上同名的目錄：p2 ↔ `p2/`、p3 ↔ `p3/`，目錄裡是該 slot 的 `kernel7.img`、device tree 與 `cmdline.txt`，由 `config.txt` 的 `os_prefix` 選擇。`deploy` 先把 rootfs、kernel 與 firmware 打包成 `out/pios-update.tar`（`cargo xtask bundle`，含各檔 sha256 的 manifest），透過 SSH 串流給 Pi 上的 `pios update apply`：rootfs 寫入閒置分割區、kernel 寫入閒置 slot 目錄，全部回讀驗證，再用 firmware 的一次性 **tryboot** 旗標重開機到新系統（`tryboot.txt` 指向新 slot）。新系統在引擎就緒後自己 commit（把 `config.txt` 的 `os_prefix` 永久指過去）；在那之前的任何失敗——kernel panic、init 崩潰、10 分鐘內引擎沒起來——都會以一般重開機收場，firmware 便讀回 `config.txt`，回到舊的 rootfs 與舊的 kernel。Docker 資料在 p4，不受影響。實機量測：整個流程（rootfs + kernel）約 1 分鐘；刻意部署一個 kernel panic 的 slot，約 20–40 秒後自動回到舊系統。

GPU firmware（`bootcode.bin`、`start_cd.elf`、`fixup_cd.dat`）是共用的：firmware 本身當掉時還沒有任何東西能回滾（實機上以 tryboot 測試另一份 `start_file` 會讓板子停住，必須斷電重開），所以 firmware 只在明確要求時更新（`PIOS_DEPLOY_FIRMWARE=1 cargo xtask deploy`），而且等新 slot commit 之後才寫入，不受回滾保護。

在 Pi 上：`pios update status` 查看兩個 slot 的狀態與版本。也接受單純的 squashfs（`pios update apply rootfs.squashfs`），此時新 slot 沿用目前的 kernel。舊版映像（kernel 放在 boot 分割區根目錄）可以直接 deploy：第一次 deploy 只會送 rootfs（舊版不認得 bundle），之後再 deploy 一次就換成 slot 版面；`os_prefix` 指向的目錄不完整時，firmware 會退回根目錄的 kernel。

### Native 引擎（`[engine] kind = "native"`）

`pios engined` 是單一 Rust daemon，在 `/var/run/docker.sock` 與 2376（mTLS，同一組憑證）上提供 docker CLI 日常需要的 Docker Engine API 子集：`run`（含 `-d`、`-it`、`--rm`、`-p`、`-v`、`-e`、`-w`、`-u`、`--restart`、`--memory`、`--cpus`、`--pids-limit`、`--cap-add/--cap-drop`、`--privileged`、`--device`、`--network host|none`、`--init`）、`ps`、`logs -f`、`exec -it`、`stop`/`kill`/`restart`/`rm`、`pull`/`images`/`rmi`、`volume`、`network`、`inspect`，以及 `docker compose`（實機以 `examples/gitea` 測試過 `up -d`、`ps`、`logs`、`down -v`、`depends_on: service_completed_successfully`）。image 直接從 registry pull（token 認證、linux/arm/v7 選擇、逐層 sha256 驗證），容器由 `pios container-init` 啟動：namespaces、overlayfs、pivot_root、Docker 預設 capability 集合、cgroup v2 限制；預設 seccomp 過濾（同 Docker：keyring、bpf、io_uring、userfaultfd、建立 user namespace 等，依 capability 放行）；bridge 網路 `pios0`（172.17.0.0/16）、使用者自訂網路（172.18–31.0.0/16，含以容器名稱/compose service 名稱解析的內建 DNS）與 iptables NAT/port 轉發。沒有 containerd、shim、runc，也沒有每個容器一個 Go 行程。

實機（Pi 2 v1.1）量測，與 Docker 引擎對照：`docker run --rm alpine true` 0.53 s（dockerd 2.25 s）；整機記憶體 29 MB（59 MB）；engined RSS 約 5 MB（dockerd + containerd 約 113 MB），每個執行中的容器另有約 1.7 MB 的 helper（大多為共用頁面）；開機後 5 s 引擎就緒。

尚未支援：`build`、swarm、plugin、一個容器連多個網路、網路間隔離、IPv6、自訂 seccomp profile、`docker cp`/`diff`/`commit`/`export`、volume 首次使用時複製 image 內容、`--tty` 下的 `tty` 指令顯示（pty 來自 host 的 devpts）。需要這些時用 `kind = "docker"`；兩個引擎都編在同一個 binary 裡，切換只改設定檔。

### Gitea（`examples/gitea`）

官方 image 只有 amd64/arm64/riscv64，Pi 2 的 32 位元 Cortex-A7 跑不動；`examples/gitea/compose.yaml` 改用 Gitea 官方的 `linux-arm-6` 執行檔（固定版本、驗 sha256），SQLite、不開 code indexer、內建 SSH server、`GOMEMLIMIT` 與 450 MB 容器上限。`docker --context pi compose up -d` 後開 `http://pios:3000` 完成安裝。實機（native 引擎）：`up -d` 約 1 分鐘（含下載 107 MB 執行檔），Gitea 常駐約 115 MB，HTTP 與 SSH（埠 2222）push 正常，重開機後自動恢復。

## 效能相關設計

- Rust：musl 靜態連結、fat LTO、`codegen-units=1`、`panic=abort`、`target-cpu=cortex-a7`、mimalloc。
- 無 systemd/udev/日誌 daemon；supervisor 是單執行緒 `poll(2)` 事件迴圈，不用 async runtime。
- Kernel：需要的驅動全部 built-in、無 initramfs、`PREEMPT_VOLUNTARY`、`HZ=250`、預設 `performance` governor，移除音效/無線/藍牙/除錯（保留 firmware framebuffer 給 HDMI console）。
- Docker：`overlay2`、`log-driver: local`（有大小上限）、`userland-proxy: false`、`live-restore`。
- TLS 用 Ed25519：Go 在 32 位元 ARM 上做 RSA 很慢，RSA-2048 每次握手要花 dockerd 約 230 ms CPU。
- 記憶體：zram（lz4）swap 吸收 image pull 的尖峰，不寫 SD 卡。
- 時鐘：Pi 沒有 RTC；開機時以 build 時間與上次儲存的時間為下限，dockerd 預設等 NTP 同步後才啟動（避免 TLS 失敗）。

評估過但未採用（實機量測）：crun 取代 runc（在這塊板子上反而較慢，且不支援 `--pid=host`）、`GOGC` 調整（差異約 6%）。

## 版本鎖定

- `kernel/VERSION` — raspberrypi/linux `rpi-6.18.y` 指定 commit
- `board/rpi2/firmware.lock` — GPU firmware 檔案 sha256
- `third_party.lock` — Docker 29.9.2 armhf 靜態版 sha256、Alpine 輔助套件（iptables、e2fsprogs、CA 憑證、dropbear、htop）
- `rootfs/busybox-applets.txt` — busybox 指令連結清單（rootfs 建置時建立）

## 狀態與後續

已完成：Phase 0–3（建置系統、開機到 Rust init、系統服務、Docker 引擎）、無頭診斷、遠端管理、SSH、A/B 更新（rootfs + kernel）與自動回滾、Rust-native 引擎。

實機（Pi 2 v1.1）量測：kernel 啟動到 `pios: engine ready` 約 19–22 s；整機記憶體使用約 57 MB，PiOS 自身約 3 MB；
`docker run --rm alpine true` 約 3 s，主要是 dockerd（Go）本身的 CPU 時間。

WSL2 建置：`bash build/setup-wsl.sh` 一次，之後 `cargo xtask all`。

後續：firmware 更新的回滾保護（需要硬體 watchdog 在 firmware 階段介入）。

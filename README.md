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
| `pios-engine-native` | Rust-native 引擎（youki）的預留位置 |
| `pios-ctl` | `pios-ctl status / mem / logs / restart / reboot`；`ctld` 以 mTLS 對外提供同一套協定 |
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
| p1 | FAT32 | 128 MiB | firmware、`kernel7.img`、`pios.toml`、`docker-tls/` |
| p2 | squashfs (lz4) | 256 MiB | 唯讀 rootfs A |
| p3 | — | 256 MiB | 預留給 A/B 更新 |
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

不重燒 SD 卡的更新（手動 A/B）：把新的 `out/rootfs.squashfs` 寫入閒置的 root 分割區（p2/p3），再把 `cmdline.txt` 的 `root=` 指過去並重開機；Docker 資料在 p4，不受影響。開不了機時，在 PC 上把 `cmdline.txt` 改回原分割區即可。

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
- `third_party.lock` — Docker 29.9.2 armhf 靜態版 sha256、Alpine 輔助套件（iptables、e2fsprogs、CA 憑證、dropbear）
- `rootfs/busybox-applets.txt` — busybox 指令連結清單（rootfs 建置時建立）

## 狀態與後續

已完成：Phase 0–3（建置系統、開機到 Rust init、系統服務、Docker 引擎）、無頭診斷、遠端管理、SSH。

實機（Pi 2 v1.1）量測：kernel 啟動到 `pios: engine ready` 約 19–22 s；整機記憶體使用約 57 MB，PiOS 自身約 3 MB；
`docker run --rm alpine true` 約 3 s，主要是 dockerd（Go）本身的 CPU 時間。

WSL2 建置：`bash build/setup-wsl.sh` 一次，之後 `cargo xtask all`。

後續：自動化 A/B OTA（`pios-update`，含開機失敗自動回滾）、Rust-native 引擎（大幅降低 `docker run` 延遲與約 120 MB 記憶體）。

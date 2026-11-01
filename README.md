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
3. 從 PC 連線（需先 `cargo xtask certs`，憑證名稱為 `pios`，請讓 `pios` 解析到 Pi 的 IP；SSH、Docker 各自怎麼運作見〈連線方式〉）：

   ```sh
   docker context create pi --docker "host=tcp://pios:2376,ca=out/certs/client/ca.pem,cert=out/certs/client/cert.pem,key=out/certs/client/key.pem"
   docker --context pi run --rm hello-world
   ```

### 換新 SD 卡重裝

只換 rootfs/kernel 用下面的 A/B 更新即可；整張卡重燒時，p4（容器、image、volume、SSH host key）會從空的開始。

1. **備份 volume**（舊卡還在跑時）。native 引擎沒有 `docker cp`，用 tar 經 docker context 串流：

   ```sh
   docker --context pi volume ls
   docker --context pi compose -f examples/gitea/compose.yaml stop
   docker --context pi run --rm -v gitea_data:/data alpine tar -C /data -cf - . > gitea-backup.tar
   tar -tvf gitea-backup.tar | head    # 確認備份有內容
   ```

   備份包含 repo、SQLite 資料庫、`app.ini` 與 Gitea 執行檔（約 107 MB）。

2. **確認設定**：在板子上直接改過的 `/boot/pios.toml` 不會回寫，要同步到 `board/rpi2/pios.local.toml`（例如 `[engine] kind = "native"`、靜態 IP、`[ssh] enabled = true`）。`out/certs/` 留著（gitignored），映像會帶同一組伺服器憑證，現有的 `docker context pi` 不用改；不要重跑 `cargo xtask certs`，除非憑證遺失。
3. `cargo xtask all`，把 `out/images/pios.img` 燒到新卡（Raspberry Pi Imager → Use custom），開機等 `pios: engine ready`。
4. SSH host key 是新的，先清掉舊紀錄：`ssh-keygen -R pios; ssh-keygen -R 192.168.1.50`。
5. **還原 volume** 再啟動：

   ```sh
   docker --context pi run --rm -i -v gitea_data:/data alpine tar -C /data -xf - < gitea-backup.tar
   docker --context pi compose -f examples/gitea/compose.yaml up -d
   ```

   備份裡已有 `app.ini`，init 不會覆蓋，之前在 `app.ini` 的修改都會保留；開 `http://pios:3000` 應直接看到原本的 repo，不會出現安裝頁。

確認新卡正常之前，舊卡先留著當退路。

縮短停機：Gitea 從步驟 1 的 `stop` 停到步驟 5 的 `up -d`，大部分時間花在 `cargo xtask all` 與燒錄。可以先做步驟 2–3 把新卡燒好，再回舊卡做步驟 1 備份、換卡，最後做步驟 4–5（前提是備份後沒人再 push）。

## 連線方式：SSH 與 Docker

### 概念：四條互不相干的線

板子上有好幾個「可以連進去」的東西。它們的埠、帳號和認證方式各自獨立，混在一起就會亂：

| 要做的事 | 指令 | 埠 | 認證 | 誰在聽 |
|---|---|---|---|---|
| 進板子開 shell、看檔案、`cargo xtask deploy` | `ssh root@pios` | 22 | SSH 金鑰，或 root 密碼（選用） | dropbear（PiOS 系統服務） |
| 管容器：`run`、`ps`、`compose`… | `docker --context pi …` | 2376 | mTLS 用戶端憑證 | dockerd 或 native 引擎 |
| 引擎掛掉時的救援：`status` / `logs` / `restart` / `reboot` | `tools/pios-remote …` | 2377 | 跟 Docker 同一組 mTLS 憑證 | `ctld`，轉給 PID 1 |
| `git push` 到 Gitea | `git@pios:2222/…` | 2222 | 你在 Gitea 網頁上登記的公鑰 | Gitea 容器內建的 SSH |

另外還有 HDMI console、`pios-boot.log` 和 ACT LED，都是給開機與錯誤診斷用的，不需要網路。

幾個容易搞混的地方：

- **Docker 不走 SSH。** `docker --context pi` 是用 TLS 憑證直接連 2376。SSH 沒開，Docker 照樣能用；SSH 的金鑰和密碼也跟 Docker 無關。
- **板子上沒有 `docker` 指令。** rootfs 只有引擎（dockerd 或 native），沒有 docker CLI。容器一律從 PC 透過 docker context 管理，不是先 SSH 進去再下指令。
- **22 和 2222 是兩台不同的 SSH server。** 22 是板子本身，登入身分是 root。2222 是 Gitea 容器，帳號是 Gitea 的使用者，跟板子的 root 沒有任何關係。
- **板子只有一個帳號：root。** 這裡說的「帳號登入」指的是 root 用密碼登入。
- **金鑰和密碼可以同時存在。** 設了密碼，金鑰登入照樣能用；`cargo xtask deploy` 就是靠金鑰（`BatchMode`）自動連線的。
- **`/boot/pios.toml` 只在開機時讀。** 改完設定一定要重開機才會生效，見下方〈改設定〉。

### SSH（板子本身，埠 22）

在 `pios.toml` 裡啟用：

```toml
[ssh]
enabled = true
port = 22
# password_hash = "$6$..."   # 選用：允許 root 用密碼登入
```

**金鑰登入**：把公鑰（例如 `~/.ssh/id_ed25519.pub` 的內容）放進 `board/rpi2/authorized_keys`，一行一把。資料的流向是：

```
board/rpi2/authorized_keys ──(cargo xtask image)──▶ /boot/authorized_keys ──(開機時複製)──▶ /run/pios/root/.ssh/authorized_keys
```

**密碼登入**：設定檔裡存的是密碼的**雜湊**，不是密碼本身。用下面任一種方式產生：

```sh
openssl passwd -6          # PC 上；會問兩次密碼，輸出 $6$... 那一整串
mkpasswd -m sha512         # 或在板子上用 busybox
```

把整串填進 `password_hash`。直接填明文密碼的話，開機時會被當成設定錯誤擋下來。開機時 init 會把雜湊寫進 `/run/pios/shadow`（`/etc/shadow` 連結到這裡，因為 rootfs 是唯讀的），並讓 dropbear 接受密碼。沒設 `password_hash` 時，root 密碼是鎖住的，dropbear 也會加上 `-s -g`，完全不接受密碼。

測試密碼登入時，強制不用金鑰：

```sh
ssh -o PubkeyAuthentication=no root@pios
```

**Host key**：板子的 SSH host key 存在 p4（`/var/lib/pios/ssh/`），A/B 更新不會變。整張卡重燒之後就會換新，`ssh` 會警告 host key 不符，先清掉舊紀錄：`ssh-keygen -R pios; ssh-keygen -R 192.168.1.50`。Windows 的 OpenSSH 和 WSL 各有自己的 `~/.ssh`（`known_hosts`、私鑰），兩邊要分別處理。

密碼登入比較方便，但 dropbear 擋暴力猜測的能力有限。只在內網開，密碼設長一點；需要從外網連的話，改用金鑰並拿掉 `password_hash`。

### Docker（埠 2376）

Docker 用 **mTLS**（雙向 TLS）：板子出示伺服器憑證，PC 出示用戶端憑證，雙方都由同一個 CA 簽發，所以不需要帳號密碼。

```
cargo xtask certs  ──▶  out/certs/ca.pem（CA）
                        out/certs/server/*.pem ──(cargo xtask image)──▶ /boot/docker-tls/   ← 板子用
                        out/certs/client/*.pem                                               ← PC 用
```

1. `cargo xtask certs` 只需要跑一次。`out/certs/` 被 gitignore，要自己保存好；重跑會產生新的 CA，舊的 docker context 就連不上了。
2. 伺服器憑證只對 `pios`、`pios.local`、`localhost` 有效，所以**要用名稱 `pios` 連線，不能直接用 IP**。在 PC 的 hosts 檔加一行，例如 `192.168.1.50 pios`（Windows：`C:\Windows\System32\drivers\etc\hosts`）。如果一定要用 IP，產生憑證時加上 `PIOS_CERT_SAN=IP:192.168.1.50`。
3. 建立 docker context（一次就好），之後每個指令加上 `--context pi`，或用 `docker context use pi` 設成預設：

   ```sh
   docker context create pi --docker "host=tcp://pios:2376,ca=out/certs/client/ca.pem,cert=out/certs/client/cert.pem,key=out/certs/client/key.pem"
   docker --context pi ps
   ```

`[engine] kind` 不管是 `docker` 還是 `native`，對外都是同一個 2376 埠、同一組憑證，切換引擎不用改 context。

`tools/pios-remote` 也用這組用戶端憑證，預設從 `~/.docker/pios/` 讀，可以把 `out/certs/client/*.pem` 複製過去，或指定 `PIOS_CERTS=out/certs/client`。

### 重開機

```sh
ssh pios pios-ctl reboot             # 或 ssh pios reboot，兩者都是正常關機流程
tools/pios-remote reboot             # 不經過 SSH，SSH 或引擎掛掉時用
```

不要直接拔電源：`/data` 上的容器資料（例如 Gitea 的資料庫）可能還沒寫進卡片。重開大約 1 分鐘，容器會自動恢復。

### 重啟容器與引擎

板子上沒有 systemd，也沒有 docker CLI。只重啟容器用 docker context；重啟引擎本身則交給 PID 1 的 supervisor：

```sh
docker --context pi compose -f examples/gitea/compose.yaml restart server   # 單一 compose service
docker --context pi restart <container>                                     # 單一容器

tools/pios-remote restart engined    # native 引擎
tools/pios-remote restart dockerd    # docker 引擎（containerd 另有自己的服務）
tools/pios-remote status             # 各服務狀態與重啟次數
tools/pios-remote logs engined       # 引擎 log
```

有開 SSH 的話，`ssh pios pios-ctl restart engined` 效果相同。引擎重啟時容器會跟著停止；設了 `restart: unless-stopped` 或 `always` 的容器會在引擎起來後自動恢復，其他容器要自己再啟動。

### 改設定（`/boot/pios.toml`）

`board/rpi2/pios.local.toml` 是設定的正本，`cargo xtask image` 會把它放進映像。板子已經在跑時，不用重燒卡，把它推上去再重開機就好。`/boot` 平常是唯讀掛載，要暫時改成可寫：

```sh
scp board/rpi2/pios.local.toml pios:/tmp/pios.toml
ssh pios 'mount -o remount,rw /boot && cp /tmp/pios.toml /boot/pios.toml && sync; mount -o remount,ro /boot'
ssh pios pios-ctl reboot
```

反過來，直接在板子上改過的 `/boot/pios.toml` 不會自動回寫，要自己同步回 `pios.local.toml`，否則下次重燒就不見了。

**順序很重要**：設定檔有未知欄位或格式錯誤時，init 會開機失敗（ACT 燈顯示錯誤，每 60 秒重開一次，原因寫在 `pios-boot.log`）。所以新版才有的設定（例如 `password_hash`）要先 `cargo xtask deploy` 更新系統，等兩個 slot 都是新版之後再寫進 `/boot/pios.toml`；不然退回舊 slot 時，舊版看不懂新欄位，就會開不起來。

### 系統更新（A/B，不需重燒 SD 卡）

```sh
cargo xtask deploy     # 先重建 pios 與 rootfs；PIOS_HOST 預設為 pios
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

**`app.ini`**：由 init 在第一次啟動時寫入 `gitea_data` volume，容器內路徑是 `/data/gitea/conf/app.ini`；在板子上是 `/data/native/volumes/gitea_data/_data/gitea/conf/app.ini`（native 引擎）或 `/var/lib/docker/volumes/gitea_data/_data/gitea/conf/app.ini`（docker 引擎）。init 只在檔案不存在時才寫，之後 `compose up -d` 不會覆蓋你的修改；`down -v` 刪掉 volume 時它會一起消失。不用 SSH 也能從 PC 修改：

```sh
docker --context pi run --rm -it -v gitea_data:/data alpine vi /data/gitea/conf/app.ini
docker --context pi compose -f examples/gitea/compose.yaml restart server
```

這個 stack 直接跑 Gitea 執行檔，沒有官方 image 那個把 `GITEA__section__KEY` 環境變數轉寫進 `app.ini` 的啟動腳本，所以設定最好直接寫在 `app.ini`。

**定期雲端備份（`examples/backup`）**：板子沒有 cron，所以用一個常駐容器（alpine + rclone）在板子上排程。每天指定時間（預設 03:00 UTC+8）用 `sqlite3 .backup` 取得一致的資料庫快照（Gitea 不用停機），再把 volume 其餘內容（repo、`app.ini`，不含執行檔與 log）串流 tar.gz 到 rclone remote，不佔 SD 卡空間，並刪除超過 `KEEP_DAYS`（預設 30）天的舊備份。用 Backblaze B2（免 OAuth、免設定檔）：建好 bucket 與 application key 後，用環境變數 `B2_ACCOUNT`、`B2_KEY`、`REMOTE=:b2:<bucket>/pios` 執行 `compose up -d`；步驟與可調參數都寫在 `examples/backup/compose.yaml` 開頭。還原時下載 `gitea.db` 與 `files.tar.gz`，解到新的 `gitea_data`（見〈換新 SD 卡重裝〉的還原步驟），並把 `gitea.db` 放回 `/data/gitea/gitea.db`。

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

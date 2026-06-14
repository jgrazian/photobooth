# Network share setup (the outbox bridge)

In phone/SMS mode the Linux booth doesn't send anything itself — it writes each
job into an **outbox folder**, and a Mac running `mac/imessage-watcher.sh` reads
that folder and texts the photo via iMessage. The two machines reach the same
folder over a network share.

The photobooth app is deliberately network-agnostic: it just writes files to the
local path in `PHOTOBOOTH_OUTBOX` (and auto-creates it if missing). It does **not**
create or configure the share — that's a one-time OS setup, described below.

Pick **one** of the two layouts:

- [Option A — Linux hosts the share (Samba)](#option-a--linux-hosts-the-share-samba)
- [Option B — Mac hosts the share (Linux mounts it)](#option-b--mac-hosts-the-share-linux-mounts-it)

Both end the same way: the booth writes to its `PHOTOBOOTH_OUTBOX` path and the
Mac watcher reads the same folder from its `PHOTOBOOTH_OUTBOX` path.

---

## Option A — Linux hosts the share (Samba)

The booth exports the outbox; the Mac mounts it. This is the default the rest of
the docs assume.

### 1. Linux booth — one-time setup

```sh
# Install Samba (Fedora shown; Debian/Ubuntu: sudo apt install samba)
sudo dnf install samba

# Create the folder the booth will write to
sudo mkdir -p /srv/photobooth/outbox
sudo chown "$USER" /srv/photobooth/outbox
```

Append the share definition to `/etc/samba/smb.conf` — see
[`smb.conf.example`](./smb.conf.example) in this folder; edit the `path` and
`valid users` lines, then:

```sh
sudo smbpasswd -a "$USER"                 # set your Samba password
sudo systemctl enable --now smbd          # start Samba now + at boot

# Open the firewall (firewalld shown)
sudo firewall-cmd --add-service=samba --permanent
sudo firewall-cmd --reload
```

Run the booth pointed at that folder:

```sh
PHOTOBOOTH_OUTBOX=/srv/photobooth/outbox cargo run --release
```

### 2. Mac — mount the share and run the watcher

Mount via Finder → **Go → Connect to Server** (⌘K) → `smb://<linux-host>/outbox`
(use the booth's hostname or IP). It mounts under `/Volumes/outbox`.

```sh
PHOTOBOOTH_OUTBOX=/Volumes/outbox ./mac/imessage-watcher.sh
```

To mount from the command line instead (and reconnect after a reboot):

```sh
mkdir -p ~/photobooth-outbox
mount_smbfs //youruser@<linux-host>/outbox ~/photobooth-outbox
PHOTOBOOTH_OUTBOX=~/photobooth-outbox ./mac/imessage-watcher.sh
```

---

## Option B — Mac hosts the share (Linux mounts it)

Here the outbox folder physically lives on the Mac, so the watcher reads a
**local** folder (slightly more reliable, no network read in the hot path) and
the Linux booth mounts the Mac's share to write into it.

### 1. Mac — share a folder

```sh
mkdir -p ~/photobooth-outbox
```

Enable File Sharing: System Settings → **General → Sharing → File Sharing** →
add `~/photobooth-outbox` under *Shared Folders*, and make sure your user has
**Read & Write**. Note the share path it shows (e.g. `smb://<mac-host>/photobooth-outbox`).

Run the watcher against the local folder:

```sh
PHOTOBOOTH_OUTBOX=~/photobooth-outbox ./mac/imessage-watcher.sh
```

### 2. Linux booth — mount the Mac share with cifs

```sh
# Debian/Ubuntu: sudo apt install cifs-utils ; Fedora: sudo dnf install cifs-utils
sudo mkdir -p /mnt/photobooth-outbox

# Store the Mac credentials out of the process list
printf 'username=<mac-user>\npassword=<mac-password>\n' | sudo tee /etc/photobooth-smb.cred >/dev/null
sudo chmod 600 /etc/photobooth-smb.cred

sudo mount -t cifs //<mac-host>/photobooth-outbox /mnt/photobooth-outbox \
    -o credentials=/etc/photobooth-smb.cred,uid=$(id -u),gid=$(id -g),file_mode=0664,dir_mode=0775
```

To mount automatically at boot, add to `/etc/fstab`:

```
//<mac-host>/photobooth-outbox  /mnt/photobooth-outbox  cifs  credentials=/etc/photobooth-smb.cred,uid=1000,gid=1000,file_mode=0664,dir_mode=0775,_netdev  0  0
```

Run the booth pointed at the mount:

```sh
PHOTOBOOTH_OUTBOX=/mnt/photobooth-outbox cargo run --release
```

---

## Verifying the bridge

Without the camera, you can confirm the folder plumbing by hand: drop a matching
pair into the outbox and watch the Mac pick it up.

```sh
# On whichever machine can see the outbox folder:
cp some-photo.png  "$PHOTOBOOTH_OUTBOX/test-1.png"
printf '+15551234567\n' > "$PHOTOBOOTH_OUTBOX/test-1.phone"   # your own number
```

The running watcher should log `SEND test-1 -> +15551234567`, deliver it via
iMessage, and move both files into `sent/`. (Write the `.png` first and the
`.phone` second — the watcher triggers on the `.phone` file.)

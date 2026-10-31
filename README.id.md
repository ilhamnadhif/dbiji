# DBiji

**Klien database SQL yang ringan, ditulis sepenuhnya dengan Rust.**

DBiji adalah alternatif native untuk DBeaver pada database SQL di macOS, Windows, dan Linux: satu
biner kecil, tanpa JVM, tanpa Electron. Terbuka dalam sekitar 0,1 detik, CPU 0 % saat diam, memakai sekitar 90 MB memori — dan
hasil query jutaan baris tidak perlu muat di memori.

[English](README.md)

![DBiji dengan tema gelap dan bahasa Indonesia](docs/screenshots/editor-dark-id.png)

## Sorotan

- **Banyak database, satu antarmuka**: PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, Oracle,
  CockroachDB, ClickHouse, dan Cassandra / ScyllaDB, semuanya dengan driver yang ditulis dalam Rust
  (tanpa Java, tanpa Oracle Client). Produk yang kompatibel punya entri sendiri dengan default yang
  sesuai: YugabyteDB, TimescaleDB, Supabase, Neon, Redshift, AlloyDB, Greenplum, QuestDB, CrateDB,
  TiDB, OceanBase, Doris, StarRocks, SingleStore, PlanetScale, Azure SQL Database, dan Amazon Keyspaces.
- **Editor SQL** dengan autocomplete sesuai skema (tabel, kolom, join, alias, CTE), template,
  parameter `:nama` / `${nama}`, `@set` / `@include` / `@export`, pemformat, outline, penanda error,
  execution plan (teks, pohon, diagram), dan riwayat query yang bisa dicari.
- **Grid data** yang men-scroll 1 juta baris di bawah 1 ms per frame: edit langsung yang disimpan
  dalam satu transaksi, navigasi foreign key, filter dan urutan di server atau klien, tampilan
  record / JSON / XML, panel nilai, grafik, dan peta untuk kolom geometri.
- **Aman secara bawaan**: tipe koneksi development / test / production, konfirmasi untuk
  `DELETE`/`UPDATE` tanpa `WHERE` dan `DROP`, koneksi read-only, sesi terpisah per editor.
- **Koneksi** lewat SSH tunnel, proxy SOCKS5 / HTTP, dan SSL. Kata sandi disimpan di keychain sistem
  (macOS Keychain, Windows Credential Manager, GNOME Keyring / KWallet) atau brankas terenkripsi
  dengan kata sandi master — tidak pernah di file. Koneksi yang terputus saat komputer tidur atau
  jaringan hilang tersambung lagi dengan sendirinya.
- **Impor** koneksi dari DBeaver, `~/.pgpass`, dan URL.
- **Fitur lain yang biasa dipakai pengguna DBeaver**: ekspor / impor (CSV, JSON, SQL, XLSX, XML,
  Parquet …), migrasi antar database, data uji, diagram ER, query builder visual, schema compare dan
  data compare (dengan output Liquibase), desainer tabel, task dan penjadwal, backup / restore,
  session dan lock manager, dashboard server, serta debugger PostgreSQL.
- **Asisten AI** (opsional, dengan kunci API Anda sendiri: Anthropic, OpenAI, atau layanan yang
  kompatibel) di panel samping: mengenal skema, menulis dan menjelaskan SQL, menyimpan percakapannya,
  dan, jika diizinkan, menjalankan query read-only sendiri.
- Antarmuka Inggris dan Indonesia, tema terang dan gelap (View → Theme atau tombol matahari / bulan di
  status bar), pintasan dan toolbar yang bisa diubah, tanpa telemetri.
- **Pembaruan otomatis** dari rilis GitHub (opsional), pasang dengan satu klik, dan catatan rilis setelah setiap pembaruan.

<p>
  <img src="docs/screenshots/data.png" width="49%" alt="Data tabel: grid dengan filter dan ORDER BY di server">
  <img src="docs/screenshots/er-diagram.png" width="49%" alt="Diagram ER database contoh">
</p>

## Instalasi

Setiap [rilis](https://github.com/ilhamnadhif/dbiji/releases/latest) dibuat untuk macOS, Windows, dan
Linux:

| Sistem | Unduhan |
| --- | --- |
| macOS 12 atau lebih baru (Apple Silicon, Intel) | `DBiji-<versi>.dmg`, atau [installer satu baris](#macos) / [Homebrew](#homebrew) |
| Windows 10 / 11 (x64) | `DBiji-<versi>-windows-x64-setup.exe` (installer) atau `-windows-x64.zip` (portable) |
| Ubuntu 22.04+, Debian 12+, Linux Mint, Pop!_OS | `DBiji-<versi>-linux-amd64.deb` (`arm64` untuk ARM) |
| Fedora, RHEL / Rocky / Alma 9+, openSUSE | `DBiji-<versi>-linux-x86_64.rpm` (`aarch64` untuk ARM) |
| Arch Linux, Manjaro, EndeavourOS | `DBiji-<versi>-linux-x86_64.pkg.tar.zst` (`aarch64` untuk ARM) |
| Linux lainnya | `DBiji-<versi>-linux-x86_64.AppImage` atau `.tar.gz` (`aarch64` untuk ARM) |

### macOS

**Installer satu baris** (disarankan):

```sh
curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/dbiji/master/scripts/install.sh | bash
```

Script ini mengunduh rilis terbaru, memeriksanya (SHA-256 yang diterbitkan GitHub, tanda tangan kode,
bundle id dan versi), memasang `DBiji.app` ke `/Applications`, lalu membukanya. Jalankan lagi untuk
memperbarui. Untuk memasang versi tertentu, berikan versinya: `… | bash -s -- v0.3.0`.

**Disk image**: unduh **`DBiji-<versi>.dmg`**, buka, lalu seret **DBiji** ke **Applications**.

> **“DBiji” tidak bisa dibuka / “Apple could not verify…”?** Build rilis belum dinotarisasi Apple,
> jadi macOS memblokir aplikasi yang diunduh lewat browser. Pilih salah satu:
>
> - **Pakai installer satu baris** di atas. File yang diunduh dengan `curl` tidak diblokir.
> - **Izinkan sekali di Settings**: klik **Done** pada peringatan, buka **System Settings → Privacy &
>   Security**, lalu klik **Open Anyway** di samping “DBiji was blocked…”.
> - **Lewat Terminal**: `xattr -dr com.apple.quarantine /Applications/DBiji.app`

### Homebrew

Dari clone repositori ini (cask-nya mengikuti setiap rilis):

```sh
brew install --cask ./packaging/homebrew/dbiji.rb
```

### Windows

Jalankan **`DBiji-<versi>-windows-x64-setup.exe`**. Installer memasang DBiji untuk user Anda (tanpa
hak administrator; pilih “all users” di dialog pertamanya untuk memasang bagi semua user),
menambahkannya ke Start menu, dan bisa dihapus di **Settings → Apps**. Untuk memperbarui, jalankan
installer versi baru.

Untuk salinan **portable**, ekstrak `DBiji-<versi>-windows-x64.zip` lalu jalankan `dbiji.exe`
(dengan `--portable <FOLDER>` agar pengaturan dan datanya disimpan di folder itu).

> **“Windows protected your PC”?** Build belum ditandatangani (code-signed), jadi SmartScreen
> memperingatkan unduhan baru: klik **More info → Run anyway**.

### Linux

Unduh paket untuk distro Anda lalu pasang:

```sh
# Ubuntu, Debian, Linux Mint, Pop!_OS
sudo apt install ./DBiji-<versi>-linux-amd64.deb
# Fedora, RHEL / Rocky / Alma
sudo dnf install ./DBiji-<versi>-linux-x86_64.rpm
# openSUSE
sudo zypper install --allow-unsigned-rpm ./DBiji-<versi>-linux-x86_64.rpm
# Arch Linux, Manjaro, EndeavourOS
sudo pacman -U ./DBiji-<versi>-linux-x86_64.pkg.tar.zst
```

Di distro lain, pakai **AppImage** (`chmod +x DBiji-*.AppImage`, lalu jalankan) atau ekstrak
**tar.gz** ke `~/.local` (programnya, entri menu, dan ikonnya):
`tar -xzf DBiji-<versi>-linux-x86_64.tar.gz -C ~/.local --strip-components=1`.

Setelah itu DBiji ada di menu aplikasi (atau jalankan `dbiji`). Dibutuhkan glibc 2.34 atau lebih baru
(Ubuntu 22.04, Debian 12, Fedora 35, RHEL 9 dan yang lebih baru), X11 atau Wayland dengan OpenGL.
Kata sandi disimpan di keyring desktop lewat Secret Service (GNOME Keyring, KWallet) dan dialog file
memakai XDG desktop portal — GNOME, KDE, dan kebanyakan desktop punya keduanya; tanpa keyring, DBiji
menyimpan kata sandi di brankasnya dengan kata sandi master. Untuk memperbarui, pasang paket versi
baru.

### Dari source

```sh
cargo build --release -p dbiji-app                     # target/release/dbiji (dbiji.exe)
scripts/bundle-macos.sh                                # macOS: dist/DBiji.app, DBiji-<versi>.dmg dan .zip
scripts/bundle-linux.sh                                # Linux: .deb, .rpm, paket Arch, AppImage dan .tar.gz
pwsh scripts/bundle-windows.ps1                        # Windows: installer dan .zip (perlu Inno Setup 6)
```

## Mulai

1. Klik **New connection** (atau **Import connections from DBeaver**) di halaman awal — atau
   **Create sample database** untuk mencoba semua fitur dengan database toko SQLite kecil.
2. Buka editor SQL dengan `⌘]`, ketik query, tekan `⌘↩`.
3. Klik dua kali tabel di navigator untuk mengedit datanya; klik header kolom untuk mengurutkan
   (naik → turun → mati); `⌘S` menyimpan perubahan.

Bahasa antarmuka dan tema diatur di **Settings → General**.

## Pembaruan

Jika Anda mengizinkan, DBiji mencari rilis baru setiap kali dibuka dan setiap 6 jam. Versi baru
membuka dialog berisi versi saat ini, versi baru, dan apa saja yang baru (jika ditemukan belakangan,
muncul tombol **Update** di toolbar tanpa mengganggu pekerjaan Anda). Di Windows, **Download**
mengunduh installer baru (jalankan di atas versi yang terpasang); di Linux membuka halaman rilis
berisi paket-paketnya. Di macOS, **Install update**:

1. mengunduh versi baru dan memverifikasinya (ukuran, SHA-256 yang diterbitkan GitHub, bundle id,
   versi dan tanda tangan kode);
2. setelah **Restart now**, menutup DBiji (skrip yang belum disimpan ditanyakan dulu), memasang
   aplikasi baru, lalu membukanya lagi; salinan sebelumnya dikembalikan jika ada yang gagal.

macOS meminta kata sandi administrator hanya jika user Anda tidak bisa mengganti aplikasinya.
Help → Check for updates… langsung memeriksa; **Skip this version** dan **Remind me later** juga
tersedia. Setelah diperbarui, DBiji menampilkan apa saja yang berubah (juga di Help → What's new;
semua catatan ada di [CHANGELOG.md](CHANGELOG.md)). Installer satu baris di atas juga bisa
memperbarui, dan pengguna Homebrew memperbarui dengan `brew upgrade --cask dbiji`. Pemeriksaan
pembaruan hanya satu permintaan anonim ke GitHub; tidak ada data tentang Anda atau database Anda yang
dikirim.

Pembaruan langsung memerlukan DBiji di `/Applications` (atau folder lain yang bisa ditulisi); versi
0.3.0 adalah versi pertama dengan updater.

## Baris perintah

```text
dbiji [FILE.sql | DATABASE.db ...]   buka skrip dan file SQLite
dbiji --execute FILE.db FILE.sql     buka lalu jalankan skrip di database itu
dbiji --run-task <id|nama>           jalankan task tersimpan tanpa jendela (exit code 0/1/2/3)
dbiji --portable <DIR>               simpan semua pengaturan dan data di DIR
dbiji --bench all                    benchmark startup, memori, grid, dan editor
```

## Pintasan keyboard

| Aksi | Pintasan |
| --- | --- |
| Eksekusi statement / pilihan | `⌘↩` |
| Eksekusi skrip | `⌥X` |
| Eksekusi di tab hasil baru | `⇧⌘↩` |
| Explain plan | `⇧⌘E` |
| Batalkan query | `⌘.` |
| Editor SQL baru | `⌘]` |
| Koneksi / skema aktif | `⌘9` / `⌘0` |
| Format SQL | `⇧⌘F` |
| Autocomplete | `⌃Space` |
| Palet perintah | `⇧⌘P` |
| Simpan skrip / data | `⌘S` |

Di Windows dan Linux, `⌘` adalah `Ctrl`, `⌥` adalah `Alt`, dan `⌃` juga `Ctrl`. Semua pintasan bisa
diubah di **Settings → Keyboard shortcuts** (preset DBiji atau DBeaver).

## Performa

Diukur pada build rilis (Apple M1 Pro) dengan `dbiji --bench all`:

| | Target | Hasil |
| --- | --- | --- |
| Ukuran biner (macOS) | < 50 MB | 44,0 MB |
| Waktu buka sampai frame pertama | < 1 dtk | ≈ 0,12 dtk |
| Memori saat diam | < 120 MB | ≈ 93 MB |
| 3 koneksi + 5 tab hasil | < 250 MB | ≈ 93 MB |
| Scroll 1 juta baris | < 16 ms / frame | p95 0,5 ms |
| Mengetik di skrip 50.000 baris | < 16 ms / frame | p95 0,5 ms |

## Keamanan dan data

- Tanpa telemetri. DBiji hanya terhubung ke server yang Anda atur dan, jika diaktifkan, ke penyedia
  AI Anda serta ke GitHub untuk pemeriksaan pembaruan (satu permintaan anonim untuk rilis terbaru
  saat DBiji dibuka dan setiap 6 jam; ditanyakan sekali, bisa diubah di Settings → General).
- Kata sandi dan kunci API disimpan di penyimpanan kredensial sistem (macOS Keychain, Windows
  Credential Manager, Secret Service di Linux) atau `vault.bin` (Argon2id + XChaCha20-Poly1305);
  file konfigurasi ditulis dengan mode 0600.
- Pengaturan, koneksi, skrip, riwayat, dan task ada di
  `~/Library/Application Support/dev.dbiji.DBiji/` di macOS, `%APPDATA%\dbiji\DBiji\` di Windows, dan
  `~/.config/dbiji/` + `~/.local/share/dbiji/` di Linux (atau `DBIJI_HOME` / `--portable`).

## Build dan pengujian

```sh
cargo test --workspace
# uji integrasi terhadap server sungguhan (Docker):
export DBIJI_TEST_PG=postgres://dbiji:dbiji@127.0.0.1:55432/dbiji
export DBIJI_TEST_MYSQL=mysql://root:dbiji@127.0.0.1:53306/dbiji
export DBIJI_TEST_MARIADB=mariadb://root:dbiji@127.0.0.1:53307/dbiji
export DBIJI_TEST_COCKROACH='cockroachdb://root@127.0.0.1:26257/defaultdb?sslmode=disable'
export DBIJI_TEST_ORACLE=oracle://dbiji:dbiji@127.0.0.1:51521/FREEPDB1   # gvenzl/oracle-free
export DBIJI_TEST_CASSANDRA=cassandra://127.0.0.1:59042                  # scylladb/scylla
cargo test -p dbiji-app -p dbiji-driver-oracle -p dbiji-driver-cassandra
```

Tanda tangan dan notarisasi: atur `DBIJI_SIGN_IDENTITY` (Developer ID) dan `DBIJI_NOTARY_PROFILE`
sebelum menjalankan `scripts/bundle-macos.sh`.

Struktur proyek ada di [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), matriks paritas DBeaver di
[docs/PARITY.md](docs/PARITY.md).

## Keterbatasan

- Hanya database yang punya driver dalam Rust yang didukung: Db2, Teradata, Informix, Snowflake,
  BigQuery, dan DuckDB tidak. Oracle memakai driver Rust buatan Oracle sendiri, yang masih beta.
- Kerberos dan autentikasi multi-faktor MySQL belum didukung.
- Build rilis belum dinotarisasi (macOS) atau ditandatangani (Windows).

## Lisensi

MIT OR Apache-2.0.

Font Inter yang disertakan (`crates/dbiji-app/assets/fonts`) berlisensi SIL Open Font License 1.1 (`Inter-LICENSE.txt`).

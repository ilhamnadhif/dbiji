# DBiji

**Klien database SQL yang ringan, ditulis sepenuhnya dengan Rust.**

DBiji adalah alternatif native untuk DBeaver pada database SQL: satu biner kecil, tanpa JVM, tanpa
Electron. Terbuka dalam sekitar 0,1 detik, CPU 0 % saat diam, memakai sekitar 70 MB memori — dan
hasil query jutaan baris tidak perlu muat di memori.

[English](README.md)

![DBiji dengan tema gelap dan bahasa Indonesia](docs/screenshots/editor-dark-id.png)

## Sorotan

- **Banyak database, satu antarmuka**: PostgreSQL, CockroachDB, MySQL, MariaDB, SQLite, SQL Server,
  ClickHouse, dan (opsional) DuckDB.
- **Editor SQL** dengan autocomplete sesuai skema (tabel, kolom, join, alias, CTE), template,
  parameter `:nama` / `${nama}`, `@set` / `@include` / `@export`, pemformat, outline, penanda error,
  execution plan (teks, pohon, diagram), dan riwayat query yang bisa dicari.
- **Grid data** yang men-scroll 1 juta baris di bawah 1 ms per frame: edit langsung yang disimpan
  dalam satu transaksi, navigasi foreign key, filter dan urutan di server atau klien, tampilan
  record / JSON / XML, panel nilai, grafik, dan peta untuk kolom geometri.
- **Aman secara bawaan**: tipe koneksi development / test / production, konfirmasi untuk
  `DELETE`/`UPDATE` tanpa `WHERE` dan `DROP`, koneksi read-only, sesi terpisah per editor.
- **Koneksi** lewat SSH tunnel, proxy SOCKS5 / HTTP, dan SSL. Kata sandi disimpan di **macOS
  Keychain** atau brankas terenkripsi dengan kata sandi master — tidak pernah di file.
- **Impor** koneksi dari DBeaver, `~/.pgpass`, dan URL.
- **Fitur lain yang biasa dipakai pengguna DBeaver**: ekspor / impor (CSV, JSON, SQL, XLSX, XML,
  Parquet …), migrasi antar database, data uji, diagram ER, query builder visual, schema compare dan
  data compare (dengan output Liquibase), desainer tabel, task dan penjadwal, backup / restore,
  session dan lock manager, dashboard server, debugger PostgreSQL, serta asisten AI opsional dengan
  kunci API Anda sendiri.
- Antarmuka Inggris dan Indonesia, tema terang dan gelap, pintasan dan toolbar yang bisa diubah,
  tanpa telemetri.

## Instalasi

Membutuhkan macOS 12 Monterey atau lebih baru.

### Disk image

Unduh **`DBiji-<versi>.dmg`** dari
[rilis terbaru](https://github.com/ilhamnadhif/dbiji/releases/latest), buka, lalu seret **DBiji** ke
**Applications**.

> **“DBiji” tidak bisa dibuka / “Apple could not verify…”?** Build rilis belum dinotarisasi Apple.
> Buka **System Settings → Privacy & Security** lalu klik **Open Anyway** di samping “DBiji was
> blocked…”, atau jalankan `xattr -dr com.apple.quarantine /Applications/DBiji.app` di Terminal.

### Homebrew

```sh
brew install --cask ./packaging/homebrew/dbiji.rb
```

### Dari source

```sh
cargo build --release -p dbiji-app                     # target/release/dbiji
cargo build --release -p dbiji-app --features duckdb   # dengan engine DuckDB tertanam
scripts/bundle-macos.sh                                # dist/DBiji.app dan dist/DBiji-<versi>.dmg
```

## Mulai

1. Klik **New connection** (atau **Import connections from DBeaver**) di halaman awal — atau
   **Create sample database** untuk mencoba semua fitur dengan database toko SQLite kecil.
2. Buka editor SQL dengan `⌘]`, ketik query, tekan `⌘↩`.
3. Klik dua kali tabel di navigator untuk mengedit datanya; klik header kolom untuk mengurutkan
   (naik → turun → mati); `⌘S` menyimpan perubahan.

Bahasa antarmuka dan tema diatur di **Settings → General**.

## Baris perintah

```text
dbiji [FILE.sql | DATABASE.db ...]   buka skrip, file SQLite / DuckDB
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

Semua pintasan bisa diubah di **Settings → Keyboard shortcuts** (preset DBiji atau DBeaver).

## Performa

Diukur pada build rilis (Apple M1 Pro) dengan `dbiji --bench all`:

| | Target | Hasil |
| --- | --- | --- |
| Ukuran biner | < 40 MB | 37 MB |
| Waktu buka sampai frame pertama | < 1 dtk | ≈ 0,1 dtk |
| Memori saat diam | < 120 MB | ≈ 69 MB |
| 3 koneksi + 5 tab hasil | < 250 MB | ≈ 83 MB |
| Scroll 1 juta baris | < 16 ms / frame | p95 0,5 ms |
| Mengetik di skrip 50.000 baris | < 16 ms / frame | p95 0,4 ms |

## Keamanan dan data

- Tanpa telemetri. DBiji hanya terhubung ke server yang Anda atur dan, jika diaktifkan, ke penyedia
  AI Anda.
- Kata sandi dan kunci API disimpan di macOS Keychain atau `vault.bin` (Argon2id +
  XChaCha20-Poly1305); file konfigurasi ditulis dengan mode 0600.
- Pengaturan, koneksi, skrip, riwayat, dan task ada di
  `~/Library/Application Support/dev.dbiji.DBiji/` (atau `DBIJI_HOME` / `--portable`).

## Build dan pengujian

```sh
cargo test --workspace --exclude dbiji-driver-duckdb
# uji integrasi terhadap server sungguhan (Docker):
export DBIJI_TEST_PG=postgres://dbiji:dbiji@127.0.0.1:55432/dbiji
export DBIJI_TEST_MYSQL=mysql://root:dbiji@127.0.0.1:53306/dbiji
export DBIJI_TEST_MARIADB=mariadb://root:dbiji@127.0.0.1:53307/dbiji
export DBIJI_TEST_COCKROACH='cockroachdb://root@127.0.0.1:26257/defaultdb?sslmode=disable'
cargo test -p dbiji-app
```

Tanda tangan dan notarisasi: atur `DBIJI_SIGN_IDENTITY` (Developer ID) dan `DBIJI_NOTARY_PROFILE`
sebelum menjalankan `scripts/bundle-macos.sh`.

Struktur proyek ada di [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), matriks paritas DBeaver di
[docs/PARITY.md](docs/PARITY.md).

## Keterbatasan

- Oracle, Db2, Firebird, H2, dan Trino belum didukung (belum ada driver jaringan Rust).
- Kerberos dan autentikasi multi-faktor MySQL belum didukung.
- Build rilis belum dinotarisasi.

## Lisensi

MIT OR Apache-2.0.

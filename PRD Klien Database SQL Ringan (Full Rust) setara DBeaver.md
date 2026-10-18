# PRD: DBiji, Klien Database SQL Ringan (Full Rust)

Oct 18, 2026 · @Tommy Lumos Empire

## 1. Ringkasan

Produk ini adalah aplikasi desktop klien database SQL yang ditulis 100% Rust, termasuk UI dan driver, dengan paritas fitur terhadap DBeaver untuk database relasional. Tujuannya menggantikan DBeaver bagi pengguna yang terganggu oleh pemakaian memori, waktu start, dan ketergantungan pada Java.

Nama proyek: **DBiji**.

**Masalah.** DBeaver sangat lengkap, tetapi dibangun di atas Java dan Eclipse/SWT sehingga membawa JVM, banyak plugin, dan driver JDBC. Akibatnya start lambat dan memori besar, terutama saat banyak koneksi dan result set terbuka.

**Peluang.** Satu binary native dengan driver Rust, grid data yang di-render hemat memori, dan hanya fitur SQL. Fokus ini memungkinkan aplikasi kecil dan responsif tanpa kehilangan alur kerja yang dipakai sehari-hari.

**Cakupan.** Hanya database SQL; NoSQL (MongoDB, Redis, Cassandra, dan sejenisnya), cloud explorer, dan fitur kolaborasi tim berada di luar lingkup. PostgreSQL dan MySQL/MariaDB adalah prioritas utama, disusul SQLite, SQL Server, dan Oracle.

**Dasar paritas.** Daftar fitur diambil dari dokumentasi resmi DBeaver dan dipetakan di bagian 4 dan 5. Aplikasi ini tidak memakai nama, logo, atau aset DBeaver.

## 2. Tujuan dan target terukur

Target utama: aplikasi dengan 3 koneksi aktif dan 5 tab hasil query memakai kurang dari 250 MB RAM, dan siap dipakai kurang dari 1 detik setelah dibuka. Angka di bawah adalah target desain awal, bukan hasil pengukuran, dan diverifikasi lewat benchmark otomatis.

| Metrik | Target | Cara ukur |
| --- | --- | --- |
| Memori idle, 1 koneksi PostgreSQL | < 120 MB | Memori fisik di Activity Monitor / `ps` |
| Memori, 3 koneksi + 5 tab hasil | < 250 MB | Skenario benchmark tetap |
| Waktu start dingin | < 1 detik | Sampai jendela bisa diketik |
| Ukuran binary | < 40 MB | Build rilis, tanpa JVM atau runtime tambahan |
| Membuka result set 1 juta baris | UI tetap responsif, memori terpakai hanya untuk baris yang ditampilkan dan di-cache | Grid virtual + fetch bertahap |
| Latensi scroll dan ketik | < 16 ms per frame | Profiling frame time |
| CPU idle | \~0% | Render hanya saat ada perubahan |

**Tujuan fungsional**

- Setiap fitur SQL pada daftar DBeaver di bagian 4 tersedia, dengan perilaku yang setara untuk PostgreSQL dan MySQL/MariaDB sebelum database lain.
- Pengguna DBeaver bisa memindahkan koneksi lewat import tanpa input ulang.
- Seluruh aplikasi, termasuk driver database, berjalan tanpa Java atau pustaka klien native bawaan database.

**Non-tujuan**

- Tidak mendukung NoSQL (MongoDB, Redis, Cassandra, dan sejenisnya), graph DB, dan time-series khusus.
- Tidak ada cloud explorer, cloud storage, dan login SSO ke AWS/Azure/GCP.
- Tidak ada edisi web, mode tim, atau berbagi koneksi antarpengguna.
- Tidak mendukung plugin Eclipse atau sistem ekstensi pihak ketiga pada versi awal.
- Tidak memakai nama, logo, atau aset DBeaver.

## 3. Target pengguna

Pengguna utama adalah developer dan DBA yang bekerja dengan PostgreSQL dan MySQL/MariaDB setiap hari dari laptop dan ingin klien yang cepat dan hemat memori.

| Persona | Kebutuhan utama | Skenario khas |
| --- | --- | --- |
| Backend developer | Menjalankan query, melihat dan mengedit data, memahami skema | Membuka 2–3 koneksi (lokal, staging, produksi read-only) dan berpindah antar tab SQL |
| DBA / data engineer | Memantau sesi dan lock, membandingkan skema, memindahkan data | Memeriksa sesi yang menggantung, membandingkan skema staging dan produksi, mengekspor tabel besar |
| Analis data | Query, filter, grafik, ekspor ke CSV/XLSX | Menjalankan query, melihat hasil sebagai grafik, mengekspor ke Excel |
| Pengguna DBeaver yang ingin beralih | Memindahkan koneksi dan skrip tanpa mengulang | Mengimpor daftar koneksi dan folder skrip SQL, lalu memakai alur kerja yang sama |

**Skenario utama**

1. Pengguna membuka aplikasi, memilih koneksi dari navigator, menekan satu pintasan untuk membuka editor SQL, menulis query dengan autocomplete, lalu menjalankannya dan melihat hasil di grid.
2. Pengguna mengedit sel di grid hasil, melihat perubahan yang tertunda, lalu menyimpan atau membatalkannya dalam satu transaksi.
3. Pengguna mengekspor tabel besar ke CSV tanpa membekukan UI.
4. Pengguna membuka diagram ER sebuah skema dan menelusuri relasi antar tabel.

## 4. Hasil riset: fitur DBeaver untuk SQL

DBeaver mendokumentasikan 14 area fitur yang relevan untuk database SQL. Tabel ini adalah inventaris yang menjadi acuan paritas; bagian NoSQL, cloud explorer, dan kolaborasi tim sudah dikeluarkan sesuai non-tujuan.

| Area | Fitur yang tercatat di dokumentasi | Sumber |
| --- | --- | --- |
| Koneksi | Wizard koneksi, tipe koneksi (dev/test/prod), SSH tunnel, SSL beserta truststore, proxy, network profile, skrip inisialisasi dan shell command, mode transaksi otomatis/manual, pending transaction dan transaction log, koneksi terpisah per editor, driver manager, impor koneksi dari tool lain | [Create connection](https://dbeaver.com/docs/dbeaver/Create-Connection/) |
| Autentikasi dan keamanan | Username/password, pgpass, password dari shell command, Kerberos, 2FA MySQL, penyimpanan rahasia aman dengan master password, batasan keamanan per koneksi (read-only), ganti password database | [Security](https://dbeaver.com/docs/dbeaver/Security/) |
| Navigator | Pohon koneksi dan objek, tampilan sederhana dan lanjutan, filter objek, bookmark, proyek dan project explorer, pencarian metadata, pencarian full-text database, pencarian data, properties editor | [Database navigator](https://dbeaver.com/docs/dbeaver/Database-Navigator/) |
| Editor SQL | Banyak skrip per koneksi yang disimpan sebagai file, eksekusi statement/skrip/pilihan, hasil di banyak tab, autocomplete dan analisis semantik (termasuk CTE), template, formatter, panel variabel, hyperlink objek, outline, indikator error, pemeriksa ejaan, SQL generation, SQL terminal, perintah sisi klien, query manager (riwayat), ganti koneksi/skema aktif | [SQL Editor](https://dbeaver.com/docs/dbeaver/SQL-Editor/) |
| Query plan dan builder | Diagram execution plan, visual query builder | [Query execution plan](https://dbeaver.com/docs/dbeaver/Query-Execution-Plan/) |
| Editor data | Tampilan grid, teks, record, spasial, grafik, JSON, XML; filter dan urutan di server atau klien; edit inline; tambah, duplikat, hapus baris dengan simpan/batal; panel Value, References, Metadata, Grouping, Calc; copy lanjutan; navigasi lewat foreign key; refresh dan auto-refresh; fetch size; hitung total baris; virtual column dan virtual key; kamus data; format data | [Data editor](https://dbeaver.com/docs/dbeaver/Data-Editor/) |
| Grafik dan data khusus | Grafik dari hasil query, dashboard, mock data generation, data spasial GIS, data XML/JSON | [Dashboards](https://dbeaver.com/docs/dbeaver/Dashboards/) |
| Diagram ER | Diagram otomatis dari skema, diagram kustom, mode edit untuk mengubah skema dari diagram | [ER diagrams](https://dbeaver.com/docs/dbeaver/ER-Diagrams/) |
| Data transfer | Ekspor ke CSV, DBUnit, JSON, Markdown, kode sumber (PHP), SQL, TXT, XML, XLSX, HTML; impor dari CSV, XLSX, XML; impor dan replace; migrasi antar database; simpan konfigurasi sebagai task | [Data export](https://dbeaver.com/docs/dbeaver/Data-export/) |
| Perbandingan | Data compare, schema compare, keluaran HTML, skrip DDL, diagram diff, dan Liquibase changelog | [Schema compare](https://dbeaver.com/docs/dbeaver/Schema-compare/) |
| Task | Task management, task scheduler, composite task, task skrip SQL | [Task management](https://dbeaver.com/docs/dbeaver/Task-Management/) |
| Administrasi | Backup dan restore, session manager, lock manager, alat maintenance, debugger PostgreSQL, dashboard kesehatan server | [Session Manager](https://dbeaver.com/docs/dbeaver/Session-Manager-Guide/) |
| Skema dan DDL | Pembuatan tabel, kolom, index, constraint, foreign key, trigger, partisi, pembuatan DDL, database contoh | [New table creation](https://dbeaver.com/docs/dbeaver/New-Table-Creation/) |
| Antarmuka | Tema, bahasa antarmuka, pintasan keyboard, kustomisasi toolbar, aksesibilitas | [User interface](https://dbeaver.com/docs/dbeaver/User-Interface-Themes/) |
| Asisten AI | Chat, saran query, penjelasan query, perbaikan error SQL, deskripsi objek, banyak penyedia model | [AI Assistant](https://dbeaver.com/docs/dbeaver/AI-Smart-Assistance/) |

**Catatan edisi.** Menurut [perbandingan resmi](https://dbeaver.com/edition/), SQL editor, visual query builder, editor data, diagram ER, execution plan, riwayat query, dan ekspor/impor ada di semua edisi PRO desktop (Lite, Enterprise, Ultimate). Mode edit ERD, mock data, debugger PostgreSQL, schema compare, data compare, task scheduler, backup, dan dashboard server hanya ada di Enterprise ke atas. Halaman [DBeaver Community](https://dbeaver.io/) mencantumkan editor data, editor SQL, editor skema, DDL, ER dasar, grafik dasar, impor/ekspor, task management, alat maintenance, serta SSH dan proxy. PRD ini mengambil gabungan keduanya sebagai target paritas, karena permintaannya adalah setara dengan DBeaver secara menyeluruh untuk SQL.

## 5. Lingkup paritas per fase

Semua fitur di bagian 4 menjadi target, dibagi tiga fase. Fase 1 sudah cukup untuk menggantikan DBeaver pada pekerjaan query dan edit data harian di PostgreSQL dan MySQL/MariaDB; fase 2 dan 3 menutup sisa paritas.

| Area | Fase 1: inti | Fase 2: lanjutan | Fase 3: paritas penuh |
| --- | --- | --- | --- |
| Database | PostgreSQL, MySQL, MariaDB, SQLite | SQL Server, DuckDB, ClickHouse, CockroachDB | Oracle, Db2, Firebird, H2, Trino, dan lainnya |
| Koneksi | Wizard, tipe koneksi, SSH tunnel, SSL, mode transaksi, penyimpanan rahasia di Keychain | Proxy, skrip inisialisasi, network profile, pgpass, impor koneksi dari DBeaver | Kerberos, 2FA MySQL, autentikasi lanjutan |
| Navigator | Pohon objek, filter, pencarian metadata, bookmark | Proyek dan folder skrip, pencarian data dan full-text | Properties editor lengkap |
| Editor SQL | Banyak tab, eksekusi statement dan skrip, hasil multi-tab, autocomplete, riwayat | Formatter, template, variabel, outline, analisis semantik, SQL generation, perintah sisi klien, SQL terminal | Execute dengan klien lokal, pemeriksa ejaan |
| Editor data | Grid virtual, edit inline, simpan dan batal, filter dan urutan, copy lanjutan, ekspor hasil | Panel Value, References, Metadata, Grouping, Calc; tampilan record, teks, JSON, XML | Virtual column dan key, kamus data, GIS, grafik, dashboard, mock data |
| Query plan | Execution plan teks | Diagram execution plan | Visual query builder |
| Diagram ER |  | Diagram ER otomatis dan kustom | Mode edit skema dari diagram |
| Skema dan DDL | Lihat DDL objek | Buat dan ubah tabel, kolom, index, constraint, trigger | Partisi, database contoh |
| Data transfer | Ekspor CSV, JSON, SQL, XLSX; impor CSV | Format ekspor lain, impor XLSX dan XML, simpan sebagai task | Migrasi antar database, impor dan replace |
| Perbandingan |  |  | Schema compare, data compare, keluaran Liquibase |
| Task |  | Task management | Scheduler, composite task, task skrip SQL |
| Administrasi |  | Session manager, lock manager | Backup dan restore, maintenance, debugger PostgreSQL, dashboard server |
| Antarmuka | Tema terang dan gelap, pintasan keyboard | Kustomisasi toolbar, lokalisasi | Aksesibilitas lengkap |
| Asisten AI |  |  | Chat dan saran query dengan kunci API pengguna sendiri |

## 6. Persyaratan fungsional

Setiap persyaratan berlaku untuk PostgreSQL dan MySQL/MariaDB kecuali disebut lain. Fase penempatan mengikuti bagian 5.

### 6.1 Koneksi

- **FR-C1** Pengguna membuat, mengubah, menduplikasi, dan menghapus koneksi lewat wizard; field umum: host, port, database, user, password, properti driver.
- **FR-C2** Tipe koneksi (development, test, production) dengan warna penanda, konfirmasi sebelum eksekusi, dan mode auto-commit yang berbeda.
- **FR-C3** SSH tunnel (password atau private key), SSL dengan sertifikat klien dan CA kustom, serta proxy SOCKS/HTTP.
- **FR-C4** Password disimpan di macOS Keychain atau di penyimpanan terenkripsi dengan master password; tidak pernah ditulis polos ke disk.
- **FR-C5** Mode transaksi auto-commit dan manual; indikator transaksi tertunda dan tombol commit/rollback; log transaksi sesi.
- **FR-C6** Opsi koneksi read-only yang menolak semua statement penulisan di sisi klien.
- **FR-C7** Skrip inisialisasi dijalankan setelah konek, serta pilihan koneksi terpisah per editor agar query panjang tidak memblokir navigator.
- **FR-C8** Impor koneksi dari DBeaver (`data-sources.json`), `pgpass`, dan koneksi URL.

### 6.2 Navigator dan objek

- **FR-N1** Pohon koneksi dengan grup, folder, dan pencarian cepat; objek dimuat malas dan di-cache.
- **FR-N2** Objek yang ditampilkan: database, skema, tabel, view, materialized view, index, constraint, trigger, sequence, fungsi, prosedur, tipe, ekstensi (PostgreSQL), dan event (MySQL).
- **FR-N3** Filter objek per koneksi (nama, pola), tampilan sederhana dan lanjutan, dan bookmark objek.
- **FR-N4** Editor objek dengan tab properti, data, DDL, dependensi, dan statistik.
- **FR-N5** Pencarian metadata lintas objek dan pencarian data di tabel yang dipilih.

### 6.3 Editor SQL

- **FR-S1** Banyak tab skrip per koneksi, tersimpan sebagai file `.sql` di folder proyek; pulihkan otomatis saat aplikasi dibuka kembali.
- **FR-S2** Eksekusi: statement di kursor, seluruh skrip, pilihan teks, dan eksekusi di tab hasil baru; pembatalan query yang sedang berjalan.
- **FR-S3** Hasil beberapa statement tampil di tab terpisah atau dalam satu tab.
- **FR-S4** Penyorotan sintaks sesuai dialek, autocomplete sadar skema (tabel, kolom, join, alias, CTE), dan hyperlink dari nama objek ke editor objek.
- **FR-S5** Pemformat SQL dengan opsi gaya, template snippet, dan panel variabel/parameter (`:nama`, `${nama}`).
- **FR-S6** Indikator error per statement, outline struktur query, dan pemeriksa ejaan komentar dan string.
- **FR-S7** Riwayat query (query manager) yang bisa dicari dengan waktu, durasi, dan jumlah baris.
- **FR-S8** Perintah sisi klien (`@set`, `@include`, `@export`) dan SQL terminal.
- **FR-S9** Execution plan: tampilan teks dan diagram (EXPLAIN, EXPLAIN ANALYZE) per database.
- **FR-S10** Pembuatan SQL dari objek (SELECT, INSERT, UPDATE, DELETE, DDL) dan visual query builder.

### 6.4 Editor data (grid)

- **FR-D1** Grid virtual: hanya baris dan kolom yang terlihat yang digambar; result set besar diambil bertahap (fetch size dapat diatur) dan bisa di-fetch seluruhnya.
- **FR-D2** Edit inline dengan validasi tipe; tambah, duplikat, dan hapus baris; perubahan tertunda ditandai warna dan disimpan atau dibatalkan sekaligus.
- **FR-D3** Indikator mengapa hasil tidak bisa diedit (tidak ada primary key, hasil dari join, koneksi read-only), dan dukungan virtual key.
- **FR-D4** Filter dan urutan lewat kolom filter, menu kolom, dan dialog filter; dieksekusi di server atau di klien.
- **FR-D5** Tampilan grid, teks, record, JSON, XML, dan spasial; panel Value, References, Metadata, Grouping, dan Calc.
- **FR-D6** Salin dan tempel biasa maupun lanjutan, navigasi lewat foreign key dengan riwayat maju/mundur, dan refresh manual maupun berkala.
- **FR-D7** Penampil nilai khusus: array, JSON/JSONB, XML, binary, gambar, UUID, interval, dan tipe geometri.
- **FR-D8** Grafik dari hasil (garis, batang, sebar, pai) dan pembangkit data uji untuk sebuah tabel.

### 6.5 Transfer data

- **FR-T1** Ekspor hasil query, tabel, atau banyak tabel ke CSV, JSON, SQL INSERT/UPSERT, XLSX, Markdown, HTML, XML, TXT, dan DBUnit; Parquet ditambahkan sebagai format ekstra di luar daftar DBeaver.
- **FR-T2** Opsi ekstraksi: gunakan baris yang sudah di-fetch, baris atau kolom terpilih, koneksi baru, dan jumlah thread; ekspor berjalan di latar belakang dengan progres dan pembatalan.
- **FR-T3** Impor CSV, XLSX, dan XML ke tabel yang ada atau baru, dengan pemetaan kolom, tebakan tipe dari sampel baris, dan opsi encoding, delimiter, serta format tanggal.
- **FR-T4** Migrasi tabel antar database (termasuk antar jenis database) dengan pemetaan tipe dan opsi muat (batch, truncate, replace).
- **FR-T5** Konfigurasi transfer bisa disimpan sebagai task.

### 6.6 Skema, diagram, dan perbandingan

- **FR-M1** Buat dan ubah tabel, kolom, index, constraint, foreign key, dan trigger lewat editor visual; perubahan tampil sebagai DDL sebelum diterapkan.
- **FR-M2** Diagram ER otomatis dari skema atau objek terpilih, diagram kustom yang disimpan, ekspor gambar, dan mode edit.
- **FR-M3** Schema compare menghasilkan skrip DDL, laporan HTML, diagram diff, dan changelog Liquibase; data compare antar dua tabel.

### 6.7 Administrasi dan task

- **FR-A1** Session manager (daftar sesi, query aktif, hentikan sesi) dan lock manager (tampilan lock dan pemblokir).
- **FR-A2** Backup dan restore memanggil alat database yang terpasang (misalnya `pg_dump`, `mysqldump`) dengan opsi umum.
- **FR-A3** Task: simpan, jalankan, dan jadwalkan operasi (ekspor, impor, skrip SQL, backup), dengan komposit task dan log eksekusi.

## 7. Dukungan database dan lapisan driver

Semua database diakses lewat satu antarmuka driver internal, sehingga fitur UI tidak bergantung pada database tertentu. Driver ditulis dengan crate Rust; kandidat crate di bawah perlu diverifikasi kematangan dan lisensinya saat fase masing-masing dimulai.

| Database | Fase | Kandidat crate | Catatan |
| --- | --- | --- | --- |
| PostgreSQL | 1 | `tokio-postgres` | Protokol Postgres penuh, termasuk TLS dan tipe array, JSONB, dan geometri |
| MySQL, MariaDB | 1 | `mysql_async` | Dua varian dengan perbedaan metadata dan dialek ditangani terpisah |
| SQLite | 1 | `rusqlite` | Engine C yang dibundel; file lokal, tanpa jaringan |
| SQL Server | 2 | `tiberius` | Protokol TDS |
| CockroachDB, Greenplum, AlloyDB, dan turunan Postgres lain | 2 | Driver PostgreSQL | Memakai protokol Postgres dengan penyesuaian metadata |
| ClickHouse | 2 | `clickhouse` | Via HTTP |
| DuckDB | 2 | `duckdb` | Engine embedded |
| Oracle | 3 | `oracle` | Membutuhkan pustaka klien Oracle; satu-satunya kandidat yang mungkin melanggar prinsip tanpa pustaka native |
| Db2, Firebird, H2, Trino, dan lainnya | 3 | Ditentukan kemudian | Hanya jika ada driver Rust yang layak atau implementasi protokol sendiri |

**Antarmuka driver (wajib untuk semua database)**

- **Koneksi dan eksekusi**: buka, tutup, ping, eksekusi statement dengan parameter, streaming baris, pembatalan, dan transaksi.
- **Metadata**: daftar database, skema, tabel, kolom, index, constraint, foreign key, trigger, fungsi, view, dan statistik, dibaca dari katalog sistem masing-masing.
- **Dialek**: daftar kata kunci dan fungsi untuk penyorotan, aturan kutip identifier, dan pemformat SQL.
- **Pemetaan tipe**: tipe database ke tipe tampilan grid dan sebaliknya, termasuk tipe khusus (array, JSON, geometri).
- **Execution plan**: perintah EXPLAIN per database dan pengurai hasilnya menjadi pohon.
- **DDL**: pembuat DDL objek dan pembuat perintah ALTER untuk editor skema.

**Prinsip full Rust.** Kode aplikasi, UI, dan driver jaringan ditulis dengan Rust. Engine embedded yang dibundel (SQLite, DuckDB) diizinkan karena itu engine database, bukan klien jaringan. Database yang hanya bisa diakses lewat pustaka klien native vendor ditunda sampai ada opsi tanpa pustaka tersebut.

## 8. Persyaratan non-fungsional

| Kategori | Persyaratan |
| --- | --- |
| Platform | Prioritas macOS 12 ke atas (Apple Silicon dan Intel). Kode dijaga lintas platform agar Windows dan Linux bisa menyusul tanpa penulisan ulang |
| Performa | Render berbasis perubahan; grid virtual untuk jutaan baris; query dan fetch berjalan di luar thread UI; UI tidak pernah membeku saat query berjalan |
| Memori | Result set tidak disimpan penuh di memori; cache baris berukuran terbatas dan bisa diatur; hasil di tab yang tidak aktif dilepas jika melebihi batas |
| Keamanan | Rahasia di Keychain atau penyimpanan terenkripsi; TLS terverifikasi secara default; tidak ada kredensial di log; file konfigurasi dengan permission 0600 |
| Privasi | Tanpa telemetri; satu-satunya koneksi keluar adalah ke server yang dikonfigurasi pengguna dan, jika diaktifkan, penyedia AI pilihan pengguna |
| Keandalan | Kegagalan satu koneksi atau tab tidak memengaruhi yang lain; koneksi terputus terdeteksi dan bisa disambung ulang; perubahan data yang belum disimpan tidak hilang tanpa konfirmasi |
| Keamanan data | Konfirmasi untuk statement berbahaya (DELETE/UPDATE tanpa WHERE, DROP) di koneksi bertipe production |
| Kompatibilitas | Versi server yang didukung: PostgreSQL 12 ke atas, MySQL 5.7 dan 8.x, MariaDB 10.x; fitur yang tidak tersedia di versi tertentu dinonaktifkan dengan pesan jelas |
| Kualitas kode | Unit test untuk parser, formatter, dan pemetaan tipe; uji integrasi terhadap PostgreSQL dan MySQL/MariaDB sungguhan di container; benchmark memori dan waktu start di CI |
| Distribusi | `.app` bertanda tangan dan ternotarisasi dalam DMG; instalasi lewat Homebrew cask |
| Lokalisasi | Antarmuka Inggris dan Indonesia; seluruh teks lewat sistem terjemahan |
| Aksesibilitas | Navigasi keyboard penuh untuk grid dan editor; dukungan pembaca layar sebagai target fase 3 |

## 9. Arsitektur dan tech stack

Satu proses Rust: thread UI hanya menggambar state dan mengirim perintah, sedangkan query, pembacaan metadata, dan transfer data berjalan di runtime async terpisah. Pemisahan ini menjaga UI tetap responsif saat hasil besar mengalir atau query berjalan lama.

&#91;embedded content: arsitektur modul · 3 lapisan, 9 komponen\]

Parser memecah skrip menjadi statement yang dikirim ke mesin query. Metadata dan mesin query tidak pernah berbicara langsung dengan database; keduanya lewat antarmuka driver yang sama untuk semua database.

| Fungsi | Kandidat crate | Catatan |
| --- | --- | --- |
| Async runtime | `tokio` | Pemisahan I/O dari thread UI |
| PostgreSQL | `tokio-postgres` | Termasuk TLS dan tipe khusus |
| MySQL, MariaDB | `mysql_async` | Metadata dan dialek per varian |
| SQLite | `rusqlite` | Engine C yang dibundel |
| SQL Server | `tiberius` | Fase 2 |
| TLS | `rustls` | Verifikasi sertifikat bawaan |
| SSH tunnel | `russh` | Penerusan port lokal ke server database |
| Parser SQL | `sqlparser` | Dasar untuk analisis; fallback tokenisasi |
| Buffer teks editor | `ropey` | Untuk teks besar dan undo yang efisien |
| Layout graf | `petgraph` | Diagram ER dan execution plan |
| Rahasia | `keyring` | macOS Keychain |
| Serialisasi | `serde`, `serde_json` | Konfigurasi, koneksi, dan ekspor JSON |
| Ekspor dan impor berkas | `csv`, `rust_xlsxwriter`, `calamine`, `parquet` | CSV, XLSX tulis dan baca, Parquet |
| Kripto | `argon2`, `chacha20poly1305` | Master password dan penyimpanan terenkripsi |
| Logging | `tracing` | Tanpa kredensial di log |
| UI | Belum diputuskan | Lihat pertanyaan terbuka di bagian 14 |

## 10. Desain UX

Tata letak mengikuti pola yang sudah dikenal pengguna DBeaver: navigator di kiri, area editor bertab di tengah, dan panel hasil di bawah editor SQL. Tujuannya agar pengguna yang beralih tidak perlu belajar ulang.

**Layout**

- Sidebar kiri: navigator koneksi dan objek dengan pencarian di atas, bisa disembunyikan; tab Proyek dan Skrip.
- Area tengah: tab untuk editor SQL, editor objek (properti, data, DDL, ER), dan diagram; tab bisa dipindah dan dibagi.
- Editor SQL: skrip di atas, panel hasil di bawah (atau berdampingan, bisa dialihkan); panel kanan opsional untuk Value, References, Metadata, Grouping, dan Calc.
- Bar status: koneksi aktif, skema aktif, mode transaksi, jumlah baris, dan waktu eksekusi.
- Mengikuti mode terang dan gelap macOS, dengan warna penanda per tipe koneksi.

**Pintasan utama (usulan bawaan, dapat diubah; tersedia keymap bergaya DBeaver)**

| Aksi | Pintasan |
| --- | --- |
| Jalankan statement di kursor | `⌘↩` |
| Jalankan seluruh skrip | `⌥X` |
| Editor SQL baru untuk koneksi terpilih | `⌘]` |
| Pilih koneksi aktif editor | `⌘9` |
| Pilih skema aktif editor | `⌘0` |
| Tampilkan atau sembunyikan panel hasil | `⌘6` |
| Format SQL | `⇧⌘F` |
| Buka palet perintah (cari objek, aksi, skrip) | `⇧⌘P` |
| Simpan perubahan data | `⌘S` |

**Prinsip**

- Setiap operasi yang berjalan lama (query, ekspor, impor, kompare) tampil di panel tugas latar belakang dan bisa dibatalkan.
- Error database ditampilkan lengkap dengan kode, pesan server, dan posisi pada query.
- Perubahan yang berisiko (hapus baris, DROP, perubahan skema) selalu menampilkan pratinjau SQL sebelum diterapkan.
- Palet perintah menjadi jalur cepat untuk hampir semua aksi.

## 11. Tahapan pengerjaan

Pengerjaan dibagi empat tahap berurutan, dan tiap tahap selesai hanya jika gerbang kualitasnya lolos. Tahap 0 membuktikan dua komponen paling berisiko, yaitu grid data dan editor SQL, sebelum fitur lain dibangun.

&#91;embedded content: roadmap · 4 tahap, 3 gerbang\]

Gerbang fase 3 adalah rilis akhir dengan paritas penuh; kriterianya ada di bagian 13. Jika Gate A gagal, keputusan mengganti toolkit UI diambil sebelum fase 1 dimulai.

## 12. Risiko dan mitigasi

Risiko terbesar ada pada grid data, editor SQL dengan autocomplete, dan keluasan dukungan database, bukan pada koneksi ke PostgreSQL atau MySQL. Karena itu tiga komponen tersebut dibuktikan lebih dulu pada fase 1.

| Risiko | Dampak | Mitigasi |
| --- | --- | --- |
| Grid virtual untuk jutaan baris sulit dibuat cepat dan hemat memori di UI Rust | Inti produk gagal memenuhi alasan dibuatnya | Prototipe grid lebih dulu dengan benchmark 1 juta baris dan 100 kolom; render kustom bila widget bawaan toolkit tidak cukup |
| Editor teks SQL (kursor, seleksi, undo, IME, scroll) tidak ada yang siap pakai | Kualitas mengetik buruk | Evaluasi crate editor yang ada; cadangan: buffer teks `ropey` dengan render sendiri; uji IME sejak awal |
| Autocomplete dan analisis semantik SQL lintas dialek | Fitur setara DBeaver tidak tercapai | Parser per dialek (`sqlparser-rs` sebagai dasar) dengan fallback tokenisasi; metadata diambil dari katalog, bukan ditebak |
| Driver Rust kurang matang untuk database non-Postgres/MySQL | Fase 3 tertunda | Antarmuka driver stabil sejak awal; database tambahan dikerjakan sebagai modul terpisah dan bisa ditunda |
| Perbedaan perilaku server (versi, ekstensi, mode SQL) | Bug khusus lingkungan | Matriks uji integrasi per versi di container; fitur dinonaktifkan jika versi tidak mendukung |
| Diagram ER dan diagram execution plan butuh layout graf | Tampilan buruk pada skema besar | Algoritma layout berlapis (Sugiyama) dengan penempatan manual yang tersimpan |
| Keamanan penyimpanan kredensial dan SSH tunnel | Kebocoran akses ke database | Hanya Keychain atau enkripsi dengan master password; `cargo audit` di CI; tinjau semua jalur yang menulis ke log |
| Paritas "sama persis" bisa meluas tanpa batas | Proyek tidak selesai | Inventaris di bagian 4 sebagai batas tetap; fitur di luar inventaris masuk PRD terpisah |
| Kemiripan dengan produk yang sudah ada | Risiko hukum atau merek | Tidak memakai nama, logo, atau aset DBeaver; fitur diimplementasikan dari perilaku yang terdokumentasi, bukan menyalin kode tanpa memeriksa lisensinya |

## 13. Kriteria keberhasilan dan rilis

Setiap fase dianggap selesai hanya jika semua kriteria di bawahnya lolos lewat uji otomatis atau benchmark yang bisa diulang.

**Gerbang fase 1 (inti)**

- [ ] Target memori, waktu start, dan latensi di bagian 2 tercapai pada benchmark tetap
- [ ] Result set 1 juta baris bisa di-scroll dan difilter tanpa UI membeku
- [ ] Query, edit data, simpan, dan batal bekerja benar di PostgreSQL, MySQL, MariaDB, dan SQLite (uji integrasi)
- [ ] Tidak ada password atau kunci tersimpan polos di disk atau log
- [ ] Pengguna bisa mengimpor koneksi dari DBeaver tanpa kehilangan field utama
- [ ] Ekspor lalu impor CSV, JSON, dan SQL menghasilkan data identik (uji round-trip)

**Gerbang fase 2 (lanjutan)**

- [ ] Seluruh fitur fase 2 di bagian 5 lolos uji fungsional
- [ ] Autocomplete sadar skema menyarankan tabel, kolom, dan join yang benar pada skema uji standar
- [ ] Diagram ER skema dengan 200 tabel tampil dan bisa digeser dengan lancar
- [ ] SQL Server dan dua database tambahan lolos uji integrasi

**Gerbang fase 3 (paritas penuh)**

- [ ] Setiap baris inventaris di bagian 4 punya fitur yang bekerja dan uji otomatis, atau tercatat alasan pengecualiannya
- [ ] Schema compare menghasilkan DDL yang, jika dijalankan, membuat dua skema identik
- [ ] Backup, restore, dan task terjadwal berjalan di PostgreSQL dan MySQL

**Metrik setelah rilis**

- Jumlah isu terbuka berlabel `crash` dan `data-loss`; targetnya nol sebelum rilis stabil.
- Perbandingan memori dan waktu start terhadap DBeaver pada skenario benchmark yang sama, diterbitkan bersama setiap rilis.

## 14. Pertanyaan terbuka

Lima keputusan di bawah menentukan arah teknis dan perlu dijawab sebelum pengerjaan dimulai.

- [ ] **Framework UI**: toolkit siap pakai (`egui`, `iced`, `slint`) atau renderer kustom berbasis GPU untuk grid dan editor? Rekomendasi awal: toolkit untuk kerangka, komponen grid dan editor SQL ditulis sendiri.
- [ ] **Cakupan platform**: hanya macOS dulu, atau Windows dan Linux sejak fase 1? PRD ini mengasumsikan macOS dulu dengan kode lintas platform.
- [ ] **Lapisan akses database**: driver langsung per database (`tokio-postgres`, `mysql_async`, dan seterusnya) atau satu abstraksi seperti `sqlx`? Rekomendasi awal: driver langsung, karena fitur metadata dan tipe khusus butuh kontrol penuh.
- [ ] **Asisten AI**: tetap di fase 3, atau dikeluarkan dari lingkup karena tidak esensial untuk tujuan ringan?
- [ ] **Lisensi dan nama**: lisensi proyek (MIT, Apache-2.0, atau tertutup) ; periksa ketersediaan nama DBiji di crates.io, GitHub, dan domain.

## 15. Sumber

Halaman berikut dibuka langsung dan menjadi dasar inventaris fitur. Data per 18 Oktober 2026.

- [DBeaver documentation: daftar isi dan ringkasan](https://dbeaver.com/docs/dbeaver/)
- [Compare DBeaver products: perbandingan edisi](https://dbeaver.com/edition/)
- [SQL Editor](https://dbeaver.com/docs/dbeaver/SQL-Editor/)
- [Data Editor](https://dbeaver.com/docs/dbeaver/Data-Editor/)
- [Data export](https://dbeaver.com/docs/dbeaver/Data-export/)
- [Data import](https://dbeaver.com/docs/dbeaver/Data-import/)

Informasi berikut hanya dari ringkasan hasil pencarian, halamannya tidak dibuka penuh: daftar fitur edisi Community di [dbeaver.io](https://dbeaver.io/) dan daftar database yang didukung di [CloudBeaver](https://dbeaver.com/docs/cloudbeaver/Supported-databases). Nama crate Rust di bagian 7, 9, dan 12 berasal dari pengetahuan umum penulis dan belum diverifikasi ke registri.

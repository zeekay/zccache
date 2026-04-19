# Source

Client library source for the zccache download daemon.

- `lib.rs` - public re-exports such as `DownloadClient`, `DownloadHandle`, and `FetchRequest`.
- `artifact.rs` - the main `fetch()` implementation, including locking, multipart downloads, validation, archive expansion, and the local HTTP test harness.

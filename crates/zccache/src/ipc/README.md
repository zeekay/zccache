# zccache-ipc

Platform IPC endpoint discovery: Unix domain sockets or Windows named pipes.

The default `IpcConnection::send` / `recv` path remains the v15 bincode daemon
wire. The running-process#234 migration hook lives beside it:
`send_prost` writes an explicit v16 prost frame, and `recv_wire` dispatches
incoming frames by protocol-version header so a server can accept either v15
bincode or v16 prost without breaking old clients.

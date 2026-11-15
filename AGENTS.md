# XA401 Bridge Rust

- This is the independent Rust implementation. The original Python application is retained in https://github.com/ryuya0124/xa401-mirakurun-bridge as reference only. Do not add or invoke a Python backend here.
- Keep credentials, actual device config, media, PCAP and runtime state outside Git. Never print credential values or unreviewed protocol exceptions.
- Bind the unauthenticated API to loopback. Validate Host/Origin, current XA401 UDN/model and media/AKE destinations; do not follow device redirects.
- Preserve certificate/signature/SRM/RTT verification. No provisioning, Move, STB deletion, firmware changes or authentication bypass.
- PC recordings/schedules and direct read-only playback of XA401 recordings are authorized. Keep these operations separate.
- Preserve original encoded video/audio. Verify completed recordings with real video/audio decode before publication; keep failed partial data.
- Do not occupy separate STB receive slots while a production bridge is serving viewers. Use the production broker for device checks.
- Run cargo fmt --check and cargo test --locked after changes. Record actual hardware evidence separately from unit tests.
- Split receiver, crypto, catalogue, recording, API and compatibility code into modules. Keep KonomiTV independent.
- Commit and push at meaningful milestones. Run the external research repository's publication check with actual credential matching before publication; never put those values in command arguments or logs.

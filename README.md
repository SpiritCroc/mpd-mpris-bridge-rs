# mpd-mpris-bridge-rs

This project exposes the MPD server API for generic media players that provide MPRIS on your desktop, which may be useful if you want to control arbitrary music players via MPD clients - at least with some basic functionality (play/pause/next/previous and view current playing song).

Note that a project like this [already existed before](https://github.com/jonjomckay/mpd-mpris-bridge), but I had some reliability issues with it, and I don't like js, so I decided to re-implement it in Rust.


## Useful resources

- [MPD protocol](https://mpd.readthedocs.io/en/latest/protocol.html)
- [MPD source code](https://github.com/MusicPlayerDaemon/MPD)
- [mpris Rust library](https://docs.rs/mpris/latest/mpris/)

# mpd-mpris-bridge-rs

This project exposes the MPD server API for generic media players that provide MPRIS on your desktop, which may be useful if you want to control arbitrary music players via MPD clients such as [MALP](https://github.com/SpiritCroc/malp) - at least with some basic functionality (play/pause/next/previous and view current playing song).

It's basically a re-implementation of [this nodejs project](https://github.com/SpiritCroc/mpd-mpris-bridge), because I had some reliability issues with it, and I don't like js.

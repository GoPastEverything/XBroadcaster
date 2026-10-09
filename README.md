# XBroadcaster

A native Windows studio for going live on X. The window, compositor, and output path are original Rust. This is not a port of OBS Studio.

OBS Studio is GPL-2.0. Translating it would not make it cheaper to run. OBS is already native C++ with hardware encoders. The cost people feel, especially in Streamlabs, comes from a browser UI, browser sources, and compositing work that keeps running when the picture is not changing.

## What it does

- Scenes with display, color, and text sources
- Desktop capture through DXGI Desktop Duplication, scaled on the GPU to 1280x720
- Microphone and desktop audio in shared WASAPI, with the system doing any rate conversion
- H.264 through a hardware Media Foundation encoder when the machine has one (NVENC, Quick Sync, or AMF)
- AAC at 44.1 kHz, which is the rate X recommends
- RTMPS publish to the account's X ingest
- OAuth 2.0 sign-in and the X Livestream API: source, broadcast, publish, end, and chat send

There is no browser source. A browser source is a second browser engine and is the main reason this class of app sits on the CPU and GPU while "idle".

## Run

```
cargo run -p xbroadcaster --release
cargo run -p xbroadcaster --release -- --check
```

`--check` captures one desktop frame, encodes it, and opens the audio devices. It does not contact X.

The first screen is **Sign in with X**. Each person approves their own account in the browser. Their token stays in `%APPDATA%\XBroadcaster` on that PC. The download has no API secret and no bearer token.

The app is a public OAuth client (PKCE). The OAuth 2.0 client id is built in. Register the X app as a native app, not a confidential one, and allow this redirect exactly:

```
http://127.0.0.1:43821/callback
```

That client id is visible during sign-in. It cannot read an account by itself. Do not put the client secret, bearer token, or stream key in the build. The stream source id is also the RTMPS stream key, so `%APPDATA%\XBroadcaster` should stay private.

## Output

1280x720, 30 fps, 4 Mbps H.264, 128 kbps AAC, keyframe every 3 seconds. X's create-source response recommends those video settings. The pipeline drops a late frame instead of queueing it.

Going live does not create the broadcast until X reports the ingest as active. That is a requirement of the API: the encoder has to be sending video first.

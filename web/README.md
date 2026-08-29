# svmscope — web frontend (Vercel)

The static UI. It talks to a svmscope **engine** (the Rust API) over HTTP; the two
are hosted separately because the engine runs real Solana programs in an embedded
SVM and needs a long-lived container, which serverless platforms can't provide.

## Deploy to Vercel (free, no card)

1. vercel.com → **Add New… → Project** → import `alizeeshan1234/svmScope`
2. **Root Directory:** `web`
3. **Framework Preset:** Other · **Build Command:** `sh build.sh` · **Output Directory:** `.`
4. Deploy.

## Pointing at a different engine

Edit `config.js` (`window.SVMSCOPE_API`), or append `?api=<url>` to the site URL —
handy for testing against a local engine:

    https://your-site.vercel.app/?api=http://127.0.0.1:3000

## Running everything locally

The Rust server serves this same page itself, so for local work you don't need
Vercel at all:

    cargo run -p svmscope-server    # → http://127.0.0.1:3000

# svmscope analytics

The operator's private usage page: reads the engine's token-gated `/stats`
and draws it. Deployed on its own, apart from the public UI, as the Vercel
project `svmscope-analytics`, behind Vercel Authentication; the engine's
`SVMSCOPE_STATS_TOKEN` gates the numbers themselves.

Deploy from this directory: `vercel --prod --yes`. `?api=<url>` points the
page at another engine, such as a local one.

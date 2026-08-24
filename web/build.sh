#!/usr/bin/env sh
# Vercel build: copy the canonical UI from ../static so there's one source of truth.
set -e
cp ../static/index.html ./index.html
echo "copied static/index.html → web/index.html"

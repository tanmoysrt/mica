# The mica website

One static page: `index.html`, `style.css` and `main.js`. No build step.

See it locally:

```bash
python3 -m http.server -d site 8000
```

Then open http://localhost:8000. The `site` workflow publishes this folder to GitHub Pages, at https://mica.tanmoysarkar.me, on each push to `develop` that changes it.

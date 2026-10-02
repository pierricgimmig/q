# q landing page

Static site: `index.html`, `style.css`, `app.js`. No build step, no dependencies beyond a Google Fonts link.

Preview locally with `python3 -m http.server -d site 8000` and open <http://localhost:8000>.

It deploys as plain files; `.github/workflows/pages.yml` publishes this folder to GitHub Pages on every push to `main`.

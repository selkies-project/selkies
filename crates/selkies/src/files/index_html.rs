/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The HTML the file index is wrapped in: the dashboard's palette and theme mirror ahead of
//! an open `<h1>`, and the footer whose `processDirectoryListing` script rewrites the heading
//! from the mount, forces file links to save, carries a `?token=` onto every link, and removes
//! the parent row at the root. The body of that function is byte-identical to the nginx
//! fancyindex footer (`addons/selkies-web-core/nginx/footer.html`), which
//! `tests/tools/file_index_footer_audit.mjs` runs over both copies.

/// Everything before the current path: `<!DOCTYPE html>` through the open `<h1>`.
pub const FILE_INDEX_HEADER: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <title>Desktop Files</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <script>
        /* Mirror the dashboard's stored theme (same origin, under "theme");
           "system" and denied storage leave the pick to the media query. */
        (function () {
            var apply = function () {
                var v = null;
                try {
                    v = localStorage.getItem('theme');
                } catch (e) { return; }
                if (v === 'system') { delete document.documentElement.dataset.theme; }
                else { document.documentElement.dataset.theme = v || 'dark'; }
            };
            apply();
            window.addEventListener('storage', apply);
        })();
    </script>
    <style>
        /* The dashboard's palette and type scale, so the file browser reads as
           part of the same session rather than a page from somewhere else.
           The script above stamps the dashboard's stored theme choice; with
           nothing stored it stamps the dark the dashboard defaults to. */
        :root {
            --accent: #a82a69;
            --accent-text: #a82a69;
            --accent-soft: rgba(168, 42, 105, 0.1);
            --bg: #f4f5f8;
            --surface: #ffffff;
            --surface-alt: #f6f7f9;
            --border-soft: #dfe2ea;
            --text: #23272f;
            --text-muted: #5b6478;
            --shadow: 0 1px 2px rgba(16, 20, 28, 0.06), 0 8px 24px rgba(16, 20, 28, 0.08);

            --container-max-width: 960px;
            --font-family: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
            --border-radius: 10px;
        }
        @media (prefers-color-scheme: dark) {
            :root:not([data-theme="light"]) {
                --accent: #ec4899;
                --accent-text: #f9a8d4;
                --accent-soft: rgba(236, 72, 153, 0.14);
                --bg: #12161d;
                --surface: #181d26;
                --surface-alt: #1f2530;
                --border-soft: #2b3441;
                --text: #e6ebf3;
                --text-muted: #aeb9c9;
                --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 8px 24px rgba(0, 0, 0, 0.35);
            }
        }
        :root[data-theme="dark"] {
            --accent: #ec4899;
            --accent-text: #f9a8d4;
            --accent-soft: rgba(236, 72, 153, 0.14);
            --bg: #12161d;
            --surface: #181d26;
            --surface-alt: #1f2530;
            --border-soft: #2b3441;
            --text: #e6ebf3;
            --text-muted: #aeb9c9;
            --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 8px 24px rgba(0, 0, 0, 0.35);
        }
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }
        body {
            font-family: var(--font-family);
            background-color: var(--bg);
            color: var(--text);
            line-height: 1.55;
            padding: 24px 0 60px;
        }
        .page-container {
            max-width: var(--container-max-width);
            margin: 0 auto;
            padding: 0 20px;
            position: relative;
        }
        h1 {
            color: var(--text);
            font-size: 1.4rem;
            font-weight: 600;
            letter-spacing: -0.01em;
            margin-bottom: 16px;
            padding: 0 52px 16px 0;
            border-bottom: 1px solid var(--border-soft);
            word-break: break-all;
        }
        h1 a {
            color: var(--accent-text);
            text-decoration: none;
        }
        h1 a:hover {
            text-decoration: underline;
        }
        hr {
            display: none;
        }
        table {
            width: 100%;
            border-collapse: separate;
            border-spacing: 0;
            margin-top: 20px;
            font-size: 0.9rem;
            background-color: var(--surface);
            border: 1px solid var(--border-soft);
            border-radius: var(--border-radius);
            overflow: hidden;
            box-shadow: var(--shadow);
        }
        thead {
            background-color: var(--surface-alt);
        }
        th {
            color: var(--text-muted);
            font-weight: 600;
            font-size: 0.75rem;
            text-transform: uppercase;
            letter-spacing: 0.04em;
        }
        th, td {
            padding: 11px 16px;
            text-align: left;
            border-bottom: 1px solid var(--border-soft);
        }
        tbody tr {
            transition: background-color 0.15s ease-in-out;
        }
        tbody tr:hover {
            background-color: var(--accent-soft);
        }
        tbody tr:last-child td {
            border-bottom: none;
        }
        td {
            color: var(--text-muted);
        }
        td a {
            color: var(--text);
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            font-weight: 500;
        }
        td a:hover {
            color: var(--accent-text);
            text-decoration: underline;
        }
        th a, th a:visited {
            color: var(--text-muted);
            text-decoration: none;
        }
        th a:hover {
            color: var(--accent-text);
            text-decoration: underline;
        }
        #reload-page-button {
            position: absolute;
            top: -2px;
            right: 20px;
            background-color: var(--surface);
            color: var(--text-muted);
            border: 1px solid var(--border-soft);
            border-radius: var(--border-radius);
            width: 34px;
            height: 34px;
            cursor: pointer;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            transition: color 0.15s ease-in-out, border-color 0.15s ease-in-out;
            z-index: 10;
        }
        #reload-page-button:hover {
            color: var(--accent-text);
            border-color: var(--accent);
        }
        #reload-page-button:focus-visible {
            outline: none;
            box-shadow: 0 0 0 3px var(--accent-soft);
        }
        #reload-page-button svg {
            width: 18px;
            height: 18px;
            fill: currentColor;
        }
        td:nth-child(1) {
            word-break: break-all;
        }
        td:nth-child(2), th:nth-child(2) {
            white-space: nowrap;
            width: 180px;
        }
        td:nth-child(3), th:nth-child(3) {
            text-align: right;
            white-space: nowrap;
            width: 100px;
        }
        /* Masked rather than painted, so one glyph follows the theme's text
           color instead of needing a copy per palette. */
        td a::before {
            display: inline-block;
            content: '';
            width: 1.05em;
            height: 1.05em;
            margin-right: 0.7em;
            flex-shrink: 0;
            background-color: var(--text-muted);
            mask-repeat: no-repeat;
            mask-size: contain;
            mask-position: center;
            -webkit-mask-repeat: no-repeat;
            -webkit-mask-size: contain;
            -webkit-mask-position: center;
        }
        td a:hover::before {
            background-color: var(--accent-text);
        }
        td a[href="../"]::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
        }
        td a[href$="/"]:not([href="../"])::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
        }
        td a:not([href$="/"]):not([href="../"])::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
        }
        footer {
            text-align: center;
            margin-top: 32px;
            padding-top: 16px;
            border-top: 1px solid var(--border-soft);
            font-size: 0.8rem;
            color: var(--text-muted);
        }
        footer p {
            margin: 0;
        }
        @media (max-width: 768px) {
            body {
                font-size: 14px;
                padding: 12px 0 40px;
            }
            .page-container {
                padding: 0 12px;
            }
            h1 {
                font-size: 1.15rem;
                padding-right: 46px;
            }
            #reload-page-button {
                right: 12px;
            }
            table {
                display: block;
                overflow-x: auto;
                -webkit-overflow-scrolling: touch;
            }
            th, td {
                padding: 10px 12px;
                white-space: nowrap;
            }
            td:nth-child(1) {
                min-width: 200px;
            }
            td:nth-child(2), th:nth-child(2) {
                min-width: 150px;
                width: auto;
            }
            td:nth-child(3), th:nth-child(3) {
                min-width: 80px;
                width: auto;
            }
        }
    </style>
</head>
<body>
    <div class="page-container">
        <button id="reload-page-button" title="Reload Page">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
                <path d="M17.65 6.35C16.2 4.9 14.21 4 12 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08c-.82 2.33-3.04 4-5.65 4-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/>
            </svg>
        </button>

        <h1>
"#;

/// Everything after the table: the footer and the listing script.
pub const FILE_INDEX_FOOTER: &str = r#"    </div> <!-- closes .page-container -->
    <footer>
        <p>&copy; <script>document.write(new Date().getFullYear())</script> Selkies.</p>
    </footer>

    <script>
        const reloadButton = document.getElementById('reload-page-button');
        if (reloadButton) {
            reloadButton.addEventListener('click', function() {
                window.location.reload();
            });
        }

        function processDirectoryListing() {
            // Shared with the nginx fancyindex footer (addons/selkies-web-core/
            // nginx/footer.html). The mount is derived from the pathname rather
            // than assumed: /api/files/ or the legacy /files/, either of them
            // possibly behind a deployment subfolder or a fronting proxy.
            const path = window.location.pathname;
            // Normalized before searching: behind a fronting proxy the root
            // can surface without its trailing slash, and both needles end
            // in one, so the raw path would simply never match.
            const normPath = path.endsWith('/') ? path : path + '/';
            // The OUTERMOST match wins, not a fixed preference: preferring
            // /api/files/ unconditionally mistook /files/api/files/ (a
            // subdirectory literally named api/files under the legacy mount)
            // for the /api/files/ root.
            const idxApi = normPath.indexOf('/api/files/');
            const idxLegacy = normPath.indexOf('/files/');
            let webPathPrefix = '/api/files/';
            let idx = idxApi;
            if (idxLegacy !== -1 && (idxApi === -1 || idxLegacy < idxApi)) {
                webPathPrefix = '/files/';
                idx = idxLegacy;
            }
            const injectedPathPrefix = window.__SELKIES_INJECTED_PATH_PREFIX__ || '';
            const diskPathPrefix = injectedPathPrefix || '';
            const h1 = document.querySelector('h1');

            if (h1 && idx !== -1) {
                const text = h1.textContent;
                const j = text.indexOf(webPathPrefix);
                if (j !== -1) {
                    const tail = text.slice(j + webPathPrefix.length);
                    h1.textContent = diskPathPrefix.replace(/\/+$/, '') + '/' + tail;
                }
            }

            // File links are forced to save (directories and sort links keep
            // their behavior): nginx-served listings send no Content-Disposition,
            // so a click in the dashboard's iframe would render inline. A
            // secure-mode token in the query rides every link, keeping the
            // listing navigable where the token cookie cannot follow.
            const sessionToken = new URLSearchParams(window.location.search).get('token');
            document.querySelectorAll('table#list td a').forEach(function(a) {
                const href = a.getAttribute('href') || '';
                if (!href || href.startsWith('?')) return;
                // A sorted listing carries its sort in every link's query,
                // the parent's included, so a directory is told by the path
                // ahead of it, and the token joins a query already there.
                if (!href.split('?')[0].endsWith('/')) {
                    a.setAttribute('download', '');
                }
                if (sessionToken) {
                    a.setAttribute('href', href + (href.includes('?') ? '&' : '?')
                        + 'token=' + encodeURIComponent(sessionToken));
                }
            });

            // At the root the parent row is an escape hatch out of the tree.
            // On the nginx /files/ mount its ../ resolves to /, the desktop
            // page, whose load registers a fresh primary client and kills
            // the running session; on /api/files/ it merely 404s at /api/.
            // Wrong either way, so remove the row outright, leaving no way
            // for a later style or script to bring it back.
            const isAtRoot = idx !== -1 && idx + webPathPrefix.length === normPath.length;
            if (isAtRoot) {
                const parentLink = document.querySelector('table#list a[href^="../"]');
                if (parentLink) {
                    const parentRow = parentLink.closest('tr');
                    if (parentRow) {
                        parentRow.remove();
                    }
                }
            }
        }

        let attempts = 0;
        const maxAttempts = 20;
        const intervalId = setInterval(function() {
            attempts++;
            const h1 = document.querySelector('h1');
            const table = document.getElementById('list');

            if (h1 && table) {
                clearInterval(intervalId);
                processDirectoryListing();
            } else if (attempts >= maxAttempts) {
                clearInterval(intervalId);
                processDirectoryListing();
            }
        }, 100);

    </script>
</body>
</html>
"#;

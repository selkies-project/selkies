// Shared by the tester pages: report what a page saw, and follow the page the
// harness names for this page's channel (`ch`, "main" unless the URL says).
const IT = {
  ch: new URLSearchParams(location.search).get('ch') || 'main',
  report(name, data) {
    return fetch('/report/' + name, {method: 'POST', body: JSON.stringify(data)}).catch(() => {});
  },
};
(() => {
  const here = location.pathname + location.search;
  setInterval(async () => {
    try {
      const next = (await (await fetch('/next/' + IT.ch, {cache: 'no-store'})).text()).trim();
      // The harness names a page of this site by its path.
      const page = next ? new URL(next, location.origin) : null;
      if (page && page.origin === location.origin && page.pathname + page.search !== here) {
        location.href = page.pathname + page.search;
      }
    } catch (e) { /* the site restarts between cells */ }
  }, 500);
})();

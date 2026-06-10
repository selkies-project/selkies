// Returns URL pathname against browser's URL even when running under
// iframe context where the pathname could be root directory `/` otherwise.
export function getRoutePrefix() {
  const pathname = window.location.pathname;
  const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
  return dirPath.replace(/\/$/, '');
}


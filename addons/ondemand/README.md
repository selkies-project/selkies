# Selkies for Open OnDemand

A [batch connect](https://osc.github.io/ood-documentation/latest/how-tos/app-development/interactive.html)
app on the basic template: the job runs `selkies-session` on the port the
portal picks, and the portal's reverse proxy (`/rnode/<host>/<port>/`) carries
the page and its WebSocket to the browser, behind the portal's own login.
Selkies runs in secure mode: the job provisions one session token, which the
view's link carries, so nobody else on the cluster network reaches the port.

```bash
sudo cp -r addons/ondemand /var/www/ood/apps/sys/selkies
```

Adapt `form.yml` to the site: the cluster, and in `selkies_command` how the
node finds Selkies (`selkies-session` from a native package or a module,
`<AppImage> selkies-session`, or `apptainer exec --nv <image> selkies-session`); `submit.yml.erb` takes what the
scheduler needs beyond the hours and the queue. The node needs a desktop, Xvfb
for the X11 backend, and `curl`; the portal's `host_regex` has to admit it, as
for every interactive app.

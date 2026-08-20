#!/bin/bash
# Exercise infra/packaging/*.sh against a genuinely read-only /repo, without a
# container runtime.
#
# The distro's absolute paths are rebased under $SB and the tools that need root
# or a network are replaced by stubs recording their argv. Everything else --
# sed, cp, mkdir, find, the real venv creation and wheel install -- runs for
# real, which is what the read-only-mount and staging-path bugs depend on. CI
# builds these packages in real containers; this is the check that runs anywhere
# and catches the same class of defect in seconds.
#
# Usage: tests/packaging/simulate.sh [wheel directory]
#   The wheel directory defaults to $REPO/dist and must hold a selkies wheel.
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WHEEL_SRC="${1:-${WHEEL_DIR:-${REPO}/dist}}"
SB="${SB:-${TMPDIR:-/tmp}/selkies-pkgsim}"

if ! ls "${WHEEL_SRC}"/selkies-*.whl >/dev/null 2>&1; then
  echo "no selkies wheel in ${WHEEL_SRC} (build one with: python3 -m build)" >&2
  exit 2
fi

chmod -R u+w "$SB/repo" 2>/dev/null
rm -rf "$SB"; mkdir -p "$SB"/{repo,dist,out,stubs,log,home} "$SB/etc/apk/keys"
cp -r "$REPO/infra" "$SB/repo/"
# only what the packaging scripts read: addons/ also holds node_modules
mkdir -p "$SB/repo/addons"
cp -r "$REPO/addons/js-interposer" "$REPO/addons/v4l2-interposer" "$SB/repo/addons/"
cp "$WHEEL_SRC"/selkies-*.whl "$SB/dist/"

# Rebase the container-absolute paths onto the sandbox, using a sentinel so an
# already-rewritten path is never rewritten a second time.
for f in "$SB"/repo/infra/packaging/*.sh; do
  sed -i -e "s#/etc/apk/keys#@SB@/etc/apk/keys#g" \
         -e "s#/pkg-root/opt/selkies#@PRS@#g" \
         -e "s#/pkg-root#@SB@/pkg-root#g" -e "s#/opt/selkies#@SB@/opt/selkies#g" \
         -e "s#/repo#@SB@/repo#g" -e "s#/dist#@SB@/dist#g" -e "s#/out#@SB@/out#g" \
         -e "s#\([ \"']\)/build#\1@SB@/build#g" \
         -e "s#@PRS@#@SB@/pkg-root/opt/selkies#g" -e "s#@SB@#$SB#g" "$f"
done
# This is the read-only bind mount the workflow gives the container.
chmod -R a-w "$SB/repo"

# The interposer scripts build a 32-bit variant wherever gcc can. Hosts without a
# multilib toolchain can point MULTILIB_SYSROOT at an unpacked one (see
# tests/README.md) and the gcc stub below hands the real compiler the flags that
# reach it, so the 32-bit branch is exercised rather than skipped.
MULTILIB_FLAGS=""
if echo 'int main(void){return 0;}' | gcc -m32 -x c - -o /dev/null 2>/dev/null; then
  echo "note: native 32-bit toolchain"
elif [ -n "${MULTILIB_SYSROOT:-}" ]; then
  MULTILIB_FLAGS="--sysroot=${MULTILIB_SYSROOT} -idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu"
  for d in "${MULTILIB_SYSROOT}"/usr/lib/gcc/*/*/32; do
    [ -d "${d}" ] && MULTILIB_FLAGS="${MULTILIB_FLAGS} -B${d}"
  done
  # shellcheck disable=SC2086  # MULTILIB_FLAGS carries whole flags, split on purpose
  if ! echo 'int main(void){return 0;}' | gcc -m32 ${MULTILIB_FLAGS} -x c - -o /dev/null 2>/dev/null; then
    echo "note: MULTILIB_SYSROOT does not yield a working 32-bit compiler" >&2
    MULTILIB_FLAGS=""
  else
    echo "note: 32-bit toolchain through MULTILIB_SYSROOT=${MULTILIB_SYSROOT}"
  fi
else
  echo "note: no 32-bit toolchain, the interposer scripts will skip their 32-bit variant"
fi
if [ -n "${MULTILIB_FLAGS}" ]; then
  cat > "$SB/stubs/gcc" <<EOF
#!/bin/sh
# Real compiler, with the flags that let -m32 find the unpacked toolchain
for a in "\$@"; do
  [ "\$a" = "-m32" ] && exec /usr/bin/gcc ${MULTILIB_FLAGS} "\$@"
done
exec /usr/bin/gcc "\$@"
EOF
  chmod +x "$SB/stubs/gcc"
fi

# Stubs for the tools that need root or a network; each records its argv.
for t in apt-get dnf apk pacman gem fpm abuild abuild-keygen makepkg useradd su chown dpkg; do
  cat > "$SB/stubs/$t" <<EOF
#!/bin/sh
echo "$t \$*" >> "$SB/log/calls"
case "$t" in
  fpm) n=stub; while [ \$# -gt 0 ]; do [ "\$1" = "--name" ] && n="\$2"; shift; done; : > "$SB/out/\${n}_0.0.0.dev0_stub.deb" ;;
  abuild) mkdir -p "$SB/build/apkrepo/build/x86_64"; : > "$SB/build/apkrepo/build/x86_64/selkies-0.0.0-r0.apk" ;;
  abuild-keygen) mkdir -p "\$HOME/.abuild"; : > "\$HOME/.abuild/simulated.rsa.pub" ;;
  makepkg) : > "$SB/out/selkies-0.0.0.dev0-1-x86_64.pkg.tar.zst" ;;
  dpkg) [ "\$1" = "--print-architecture" ] && echo amd64 ;;
  su) shift 2; exec /bin/sh -c "\$1" ;;
esac
exit 0
EOF
  chmod +x "$SB/stubs/$t"
done

# Each packager orders a PEP 440 pre-release only in its own spelling, so the
# version each one is handed is checked along with the staging.
declare -A WANT_VERSION=([deb]="0.0.0~dev0" [rpm]="0.0.0~dev0" [apk]="0.0.0_alpha0" [arch]="0.0.0.dev0")

failures=0
STAMP="$SB/log/stamp"; : > "$STAMP"
for name in deb rpm apk arch; do
  : > "$SB/log/calls"; sleep 1; : > "$STAMP"
  # A sandbox HOME keeps the signing keys and caches out of the real one
  ( PATH="$SB/stubs:$PATH" HOME="$SB/home" SELKIES_VERSION=0.0.0.dev0 DISTRO_TAG=stub \
    sh "$SB/repo/infra/packaging/$name.sh" ) > "$SB/log/$name.out" 2>&1
  rc=$?
  leaked="$(find "$SB/repo" -newer "$STAMP" 2>/dev/null | head -3)"
  shebang="$(head -1 "$SB/pkg-root/opt/selkies/bin/selkies" 2>/dev/null)"
  # A pip install and a native package have to put the same commands on PATH.
  # The links resolve at the install path, so they dangle until then.
  unlinked="$(cd "$SB/pkg-root/opt/selkies/bin" 2>/dev/null &&
    for c in selkies*; do [ -L "$SB/pkg-root/usr/bin/$c" ] || printf '%s ' "$c"; done)"
  case "$name" in
    # fpm takes the version on its command line, abuild and makepkg from the
    # recipe the script stages
    deb|rpm) got="$(sed -n 's/.*--version \([^ ]*\).*/\1/p' "$SB/log/calls" | head -1)" ;;
    apk) got="$(sed -n 's/^pkgver=//p' "$SB/build/apk/APKBUILD" 2>/dev/null)" ;;
    arch) got="$(sed -n 's/^pkgver=//p' "$SB/build/arch/PKGBUILD" 2>/dev/null)" ;;
  esac
  [ "$got" = "${WANT_VERSION[$name]}" ] && badver="" || badver="want ${WANT_VERSION[$name]}"
  printf '%-10s exit=%-3s repo-writes=%-5s version=%-13s commands=%-9s shebang=%-58s out=%s\n' \
    "$name" "$rc" "$([ -z "$leaked" ] && echo none || echo LEAK)" \
    "${got:-<none>}" "$([ -z "$unlinked" ] && echo linked || echo UNLINKED)" \
    "${shebang:-<none>}" "$(find "$SB/out" -mindepth 1 -maxdepth 1 -printf '%f ' 2>/dev/null)"
  [ -z "$badver" ] || echo "       | version ${got:-<none>}, ${badver}" >&2
  [ -z "$unlinked" ] || echo "       | not on PATH: ${unlinked}" >&2
  if [ "$rc" -ne 0 ] || [ -n "$leaked" ] || [ -n "$badver" ] || [ -n "$unlinked" ]; then
    failures=$((failures + 1))
    tail -6 "$SB/log/$name.out" | sed 's/^/       | /'
  fi
  chmod -R u+w "$SB/build" 2>/dev/null
  rm -rf "$SB/out" "$SB/pkg-root" "$SB/opt" "$SB/build"; mkdir -p "$SB/out"
done
chmod -R u+w "$SB/repo"
if [ "$failures" -ne 0 ]; then
  echo "packaging simulation: ${failures} script(s) failed" >&2
  exit 1
fi
echo "packaging simulation: all scripts staged cleanly"

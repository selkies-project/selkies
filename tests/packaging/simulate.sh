#!/bin/bash
# Exercise infra/packaging/*.sh against a genuinely read-only /repo, without a
# container runtime.
#
# The distro's absolute paths are rebased under $SB and the tools that need root
# or a network are replaced by stubs recording their argv. The server's build is
# one of them: a stub cargo writes a stand-in binary that reports the version
# its manifest was stamped with, and the codec and dependency resolution
# (codecs.sh, runtime-deps.sh) are stubbed too, since what they read is the
# distribution's. Everything else -- sed, cp, mkdir, find, the manifest
# stamping, the staging of the server, the web client, and the interposers --
# runs for real, which is what the read-only-mount, staging-path, and version
# bugs depend on. CI builds these packages in real containers; this is the
# check that runs anywhere and catches the same class of defect in seconds.
#
# Usage: tests/packaging/simulate.sh
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
SB="${SB:-${TMPDIR:-/tmp}/selkies-pkgsim}"

chmod -R u+w "$SB/repo" 2>/dev/null
rm -rf "$SB"; mkdir -p "$SB"/{repo,out,stubs,log,home} "$SB/etc/apk/keys" "$SB/etc/apt/apt.conf.d"
cp -r "$REPO/infra" "$REPO/crates" "$REPO/Cargo.toml" "$REPO/Cargo.lock" "$SB/repo/"
# only what the packaging scripts read: addons/ also holds node_modules
mkdir -p "$SB/repo/addons" "$SB/repo/dist/web"
cp -r "$REPO/addons/input-interposer" "$REPO/addons/v4l2-interposer" "$SB/repo/addons/"
echo '<!doctype html>' > "$SB/repo/dist/web/index.html"
# shellcheck disable=SC2016  # the stub's own arguments, expanded when it runs
printf '#!/bin/sh\nmkdir -p "$1"\n: > "$1/env.sh"\n' > "$SB/repo/infra/packaging/codecs.sh"
printf '#!/bin/sh\necho libstub1\necho libstub2\n' > "$SB/repo/infra/packaging/runtime-deps.sh"

# Rebase the container-absolute paths onto the sandbox, using a sentinel so an
# already-rewritten path is never rewritten a second time.
for f in "$SB"/repo/infra/packaging/*.sh; do
  sed -i -e "s#/etc/apk/keys#@SB@/etc/apk/keys#g" \
         -e "s#/etc/apt/apt.conf.d#@SB@/etc/apt/apt.conf.d#g" \
         -e "s#/pkg-root#@SB@/pkg-root#g" \
         -e "s#/repo#@SB@/repo#g" -e "s#/out#@SB@/out#g" \
         -e "s#\([ \"'{-]\)/build#\1@SB@/build#g" \
         -e "s#@SB@#$SB#g" "$f"
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

# The stand-in Cargo: `fetch` fetches nothing, `metadata` names a pixelflux
# checkout, and `build` leaves a server in the target directory reporting the
# workspace version the manifest it was handed carries.
cat > "$SB/stubs/cargo" <<EOF
#!/bin/sh
echo "cargo \$*" >> "$SB/log/calls"
case "\$1" in
  fetch) exit 0 ;;
  metadata) echo '{"packages":[{"name":"pixelflux","manifest_path":"$SB/repo/crates/pixelflux/Cargo.toml"}]}'; exit 0 ;;
esac
manifest=""
while [ \$# -gt 0 ]; do [ "\$1" = --manifest-path ] && manifest="\$2"; shift; done
version="\$(sed -n 's/^version = "\\(.*\\)"/\\1/p' "\${manifest}" | head -1)"
mkdir -p "\${CARGO_TARGET_DIR}/release"
printf '#!/bin/sh\\necho "selkies %s (stub)"\\n' "\${version}" > "\${CARGO_TARGET_DIR}/release/selkies"
chmod +x "\${CARGO_TARGET_DIR}/release/selkies"
EOF
chmod +x "$SB/stubs/cargo"

# Stubs for the tools that need root or a network; each records its argv.
for t in apt-get dnf apk pacman gem fpm abuild abuild-keygen makepkg useradd su chown dpkg; do
  cat > "$SB/stubs/$t" <<EOF
#!/bin/sh
echo "$t \$*" >> "$SB/log/calls"
case "$t" in
  fpm) n=stub; f=deb; while [ \$# -gt 0 ]; do case "\$1" in --name) n="\$2" ;; -t) f="\$2" ;; esac; shift; done
       case "\$f" in rpm) : > "$SB/out/\${n}-0.0.0.dev0-1.stub.x86_64.rpm" ;; *) : > "$SB/out/\${n}_0.0.0.dev0_stub.deb" ;; esac ;;
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
  # The server and its web client where the server looks for one, and the
  # dependencies the build resolved, which fpm takes as flags and abuild and
  # makepkg from the recipe the script stages
  staged=""
  [ -x "$SB/pkg-root/usr/bin/selkies" ] && [ -f "$SB/pkg-root/usr/share/selkies/web/index.html" ] && staged=1
  case "$name" in
    deb|rpm) got="$(sed -n 's/.*--version \([^ ]*\).*/\1/p' "$SB/log/calls" | head -1)"
             deps="$(grep -c -- '--depends libstub1 --depends libstub2' "$SB/log/calls")" ;;
    apk) got="$(sed -n 's/^pkgver=//p' "$SB/build/apk/APKBUILD" 2>/dev/null)"
         deps="$(grep -c '^depends="libstub1 libstub2"$' "$SB/build/apk/APKBUILD" 2>/dev/null)" ;;
    arch) got="$(sed -n 's/^pkgver=//p' "$SB/build/arch/PKGBUILD" 2>/dev/null)"
          deps="$(grep -c '^depends=("libstub1" "libstub2")$' "$SB/build/arch/PKGBUILD" 2>/dev/null)" ;;
  esac
  [ "$got" = "${WANT_VERSION[$name]}" ] && badver="" || badver="want ${WANT_VERSION[$name]}"
  printf '%-10s exit=%-3s repo-writes=%-5s version=%-13s server=%-8s depends=%-9s out=%s\n' \
    "$name" "$rc" "$([ -z "$leaked" ] && echo none || echo LEAK)" "${got:-<none>}" \
    "$([ -n "$staged" ] && echo staged || echo MISSING)" \
    "$([ "${deps:-0}" -ge 1 ] && echo declared || echo MISSING)" \
    "$(find "$SB/out" -mindepth 1 -maxdepth 1 -printf '%f ' 2>/dev/null)"
  [ -z "$badver" ] || echo "       | version ${got:-<none>}, ${badver}" >&2
  if [ "$rc" -ne 0 ] || [ -n "$leaked" ] || [ -n "$badver" ] || [ -z "$staged" ] || [ "${deps:-0}" -lt 1 ]; then
    failures=$((failures + 1))
    tail -6 "$SB/log/$name.out" | sed 's/^/       | /'
  fi
  chmod -R u+w "$SB/build" "$SB/pkg-root" 2>/dev/null
  rm -rf "$SB/out" "$SB/pkg-root" "$SB/build"; mkdir -p "$SB/out"
done
chmod -R u+w "$SB/repo"
if [ "$failures" -ne 0 ]; then
  echo "packaging simulation: ${failures} script(s) failed" >&2
  exit 1
fi
echo "packaging simulation: all scripts staged cleanly"

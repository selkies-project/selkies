# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

FROM python:3-alpine

ARG PIP_BREAK_SYSTEM_PACKAGES="1"
RUN pip3 install --no-cache-dir --retries 5 --timeout 60 --force-reinstall flask gunicorn

COPY app.py /etc

WORKDIR /etc

# Gunicorn environment variables, number of workers
ENV WEB_CONCURRENCY="3"
# Replace environment with customized TURN server
ENV TURN_SHARED_SECRET="openrelayprojectsecret"
ENV TURN_HOST="staticauth.openrelay.metered.ca"
ENV TURN_PORT="443"
ENV TURN_PROTOCOL="udp"
ENV TURN_TLS="false"
ENV STUN_HOST="stun.l.google.com"
ENV STUN_PORT="19302"

EXPOSE 8008

CMD ["sh", "-c", "python -m gunicorn --worker-class=gthread --workers=1 --threads=${WEB_CONCURRENCY:-3} --bind=0.0.0.0:8008 app:app"]

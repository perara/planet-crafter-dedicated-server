# The admin page behind a TLS reverse proxy

The admin API and web page speak plain HTTP, and the token travels in every request. Whoever reads
one request has the token, and **the token is more than a dashboard login**: it sets rules, and the
rule `chat.exec` runs a program on the server's machine as the server's user. So the page is not
for the open Internet, and not for an untrusted network either. By default it listens on
`127.0.0.1:7782` and nothing else can reach it. Three ways to use it from elsewhere, in the order
to prefer them:

1. **An SSH tunnel**, nothing to install on the server: `ssh -L 7782:127.0.0.1:7782 you@server`,
   then `http://127.0.0.1:7782/` on your own machine. The README shows it.
2. **A VPN** (WireGuard, Tailscale) that puts you on a network of your own. Bind the admin address
   to the VPN address: `PCDS_ADMIN=10.8.0.1:7782` (`pcds config`, `pcds restart`).
3. **A TLS reverse proxy** on the same machine, which is the rest of this page. The proxy is the
   only thing that listens on a public address; the admin API stays on `127.0.0.1`. In Docker, publish the port
   on the loopback (`127.0.0.1:7782:7782`, as [compose.yaml](../compose.yaml) does) and let the proxy
   reach it there, or run the proxy in the same Compose project and send it to `pcds:7782`.

If `PCDS_ADMIN` binds anything but the loopback, the server says so at every start, loudly, `pcds
doctor` warns, and it limits what one address can hold (below). It is allowed, because a private
network or a container is a fine place for it, but it is the one setting in `server.env` that
puts the machine in the hands of whoever can read the network.

## What the proxy has to do

* **Terminate TLS** and add `Strict-Transport-Security`.
* **Pass `Authorization`** and the request as it is. The server answers its own security headers
  (`Content-Security-Policy`, `X-Frame-Options: DENY`, `Cache-Control: no-store`); do not cache,
  and do not strip them.
* **Send a request body with a `Content-Length`.** The server refuses a chunked body (`411`). An
  HTTP/2 client or a streaming upload can reach the proxy chunked; Caddy's `request_buffers` and
  nginx's default buffering turn that into a length. At most 64 KiB (head and body) are read; let the
  proxy refuse more (`client_max_body_size 64k`, Caddy's `request_body`).
* **Not buffer the answer** of a long poll (`GET /api/chat/events?wait=30`, `POST
  /api/plugins/events` with `wait`): the answer comes when an event does.
* **Wait long enough:** the world controls (`/api/world/stop`, `start`, `switch`) answer up to
  200 seconds after the request, and the long polls up to 30. 220 seconds on the upstream read.
* **Leave the paths alone.** One backend, no rewriting.

## Caddy

Tried with Caddy 2.10 in front of a real server. Replace `localhost:8443` with your domain (Caddy
then gets a certificate by itself and `tls internal` goes) and `10.0.2.2:8541` with
`127.0.0.1:7782` (the address the proxy reaches the admin API on).

```
pcds.example.com {
	# The admin API reads at most 64 KiB (head and body).
	request_body {
		max_size 64KB
	}
	reverse_proxy 127.0.0.1:7782 {
		# Long polls must not wait in a buffer.
		flush_interval -1
		# The server wants a Content-Length: Caddy reads a small body whole and sends it with one.
		request_buffers 64KB
		transport http {
			dial_timeout 3s
			# The world controls are answered up to 200 s later.
			response_header_timeout 220s
		}
	}
	header Strict-Transport-Security "max-age=31536000"
}
```

## nginx

Tried with nginx 1.29 in front of a real server. Certificates from Let's Encrypt (`certbot`) or your own.

```nginx
server {
	listen 443 ssl;
	server_name pcds.example.com;
	ssl_certificate     /etc/letsencrypt/live/pcds.example.com/fullchain.pem;
	ssl_certificate_key /etc/letsencrypt/live/pcds.example.com/privkey.pem;
	ssl_protocols TLSv1.2 TLSv1.3;
	# The admin API reads at most 64 KiB (head and body); nginx buffers a request body and sends a Content-Length.
	client_max_body_size 64k;
	add_header Strict-Transport-Security "max-age=31536000" always;
	location / {
		proxy_pass http://127.0.0.1:7782;
		proxy_http_version 1.1;
		proxy_set_header Host $host;
		# Long polls must not wait in a buffer.
		proxy_buffering off;
		proxy_connect_timeout 3s;
		# The world controls are answered up to 200 s later.
		proxy_read_timeout 220s;
		proxy_send_timeout 30s;
	}
}
```

What was run against both: `GET /api/health` answered `200 {"ok":true}`; `GET /api/status` without a
token `401`, with it `200`; the page and its four security headers came through; a 70 KB body was
refused by the proxy with `413`; a request with `Transfer-Encoding: chunked` was accepted (the proxy
sent it on with a length); HTTP/2 worked with Caddy; and a chat long poll (`wait=15`) was answered
5.9 s after it began, when a player's message arrived, not at the end of the wait.

## Slow clients and limits

The admin port serves 64 connections at once. A client that opens connections and says nothing
holds each for the 2 seconds the server waits for a request head. Against that:

* From **another machine** the server serves at most 16 connections at a time to one address and 48 to
  all other machines together; past that a connection gets `503` with `Retry-After: 5` at once. The 16
  that remain are for this machine only (the command line, a proxy on the loopback), which no one
  elsewhere can use up.
* Behind a proxy **on the same machine** every connection comes from `127.0.0.1`, which the server
  trusts, so these limits do not tell clients apart: the proxy has to. nginx:

  ```nginx
  limit_conn_zone $binary_remote_addr zone=pcds_conn:1m;
  limit_req_zone  $binary_remote_addr zone=pcds_req:1m rate=20r/s;
  # in the server block:
  limit_conn pcds_conn 16;
  limit_req  zone=pcds_req burst=40 nodelay;
  client_header_timeout 5s;
  client_body_timeout 10s;
  ```

  Caddy has `servers { timeouts { read_header 5s } }` in its global options and per-client rate
  limits in a module (`caddy-ratelimit`). Both snippets pass `nginx -t` and `caddy validate`. A firewall in front of both (`ufw limit`, `nft` meters) is the plainest answer.
* Keep what the proxy lets through narrow when you can: an IP allow-list for the whole page (`allow
  203.0.113.0/24; deny all;` in nginx, `@home remote_ip 203.0.113.0/24` in Caddy) and, for a monitor
  outside, a location that passes only `/api/health` (and `/api/metrics` for the scraper's address;
  [monitoring.md](monitoring.md)).

## Checking it

```bash
curl -fsS https://pcds.example.com/api/health                       # {"ok":true}
curl -sS -o /dev/null -w '%{http_code}\n' https://pcds.example.com/api/status        # 401
curl -fsS -H "Authorization: Bearer $(pcds token)" https://pcds.example.com/api/status | head -c 200
pcds doctor                                                         # admin exposure: the admin API listens on 127.0.0.1
```

`pcds doctor` reads the server's own setting, not the proxy's: it is content when the admin API is on
the loopback, and cannot know whether a proxy stands in front of it.

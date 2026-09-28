# releaseway

A pull-through cache of release and archive downloads in an S3 bucket (R2,
S3, or anything that speaks S3's API). `GET /<host>/<path>` answers with
`https://<host>/<path>`: from the bucket when it has it, and otherwise
fetched once, stored, and answered from there.

By default it caches GitHub's download hosts: `github.com`,
`codeload.github.com`, `objects.githubusercontent.com` and
`release-assets.githubusercontent.com`, which is where a repository's
archives and release assets come from. It was made for Bazel, whose
rulesets and toolchains are mostly GitHub archives: pointed at releaseway,
a build keeps working while GitHub is down, after a tag's archive has gone,
or on a machine that cannot reach GitHub at all.

- **Only the configured hosts**, and only paths that could name a file
  there: no query, no empty, `.` or `..` segments. Anything else is a 404,
  and a redirect to another host is refused.
- **Only fetched with the token.** A file the bucket has not got yet is
  fetched only for a caller with the token, as a bearer token or as the
  password of Basic auth, which is what `~/.netrc` gives Bazel, curl and
  most other tools. Without it the answer is 401, so nobody else can fill
  the bucket, or have it hold something of theirs under your name. What is
  cached already is anybody's, as it is upstream.
- **Never fetched again.** Tools that name a download by its hash (Bazel's
  `sha256`, a lockfile's integrity) fail a file that changed upstream
  whichever copy it came from, so the first copy is the one to keep.
- **One file at a time through a scratch folder**, never whole in memory,
  up to 2 GiB each by default.
- **Up without a bucket**, answering 503 and saying why in its log, so a
  deploy made before its Secret still rolls out.

## Using it with Bazel

A downloader config, passed with `--experimental_downloader_config`, that
tries releaseway first and GitHub second. When two rewrites match one URL,
Bazel tries both, in order, so a cache that is down costs one failed
request:

```
rewrite (github\.com|codeload\.github\.com|objects\.githubusercontent\.com|release-assets\.githubusercontent\.com)/(.*) cache.example.com/$1/$2
rewrite (github\.com|codeload\.github\.com|objects\.githubusercontent\.com|release-assets\.githubusercontent\.com)/(.*) $1/$2
```

and, on a machine that may add to the cache, `~/.netrc` (Bazel reads it for
rewritten URLs too):

```
machine cache.example.com
login bazel
password <the token>
```

## Configuration

| Variable | |
| --- | --- |
| `RELEASEWAY_BUCKET_ENDPOINT` | e.g. `https://<account id>.r2.cloudflarestorage.com` |
| `RELEASEWAY_BUCKET` | the bucket's name |
| `RELEASEWAY_BUCKET_REGION` | `auto` (R2's) unless set |
| `RELEASEWAY_ACCESS_KEY_ID`, `RELEASEWAY_SECRET_ACCESS_KEY` | a key that can read and write the bucket |
| `RELEASEWAY_TOKEN` | what a caller must send for anything to be fetched; without it, only what is cached is served |
| `RELEASEWAY_HOSTS` | the hosts cached, comma-separated; GitHub's four unless set |
| `RELEASEWAY_MAX_BYTES` | the largest file fetched; 2 GiB unless set |
| `RELEASEWAY_SCRATCH` | where a download waits on its way to the bucket; `/tmp` unless set |
| `LISTEN_ADDR` | `0.0.0.0:8080` unless set |

It logs JSON lines on stdout, one per request, and stops on SIGTERM.
`GET /healthz` is for a readiness probe.

## Kubernetes

The chart is published to `oci://ghcr.io/u9g/charts/releaseway` with every
release, and its image to `ghcr.io/u9g/releaseway`:

```sh
kubectl create secret generic releaseway \
  --from-literal=ACCESS_KEY_ID=... --from-literal=SECRET_ACCESS_KEY=... \
  --from-literal=TOKEN="$(openssl rand -hex 32)"
helm install releaseway oci://ghcr.io/u9g/charts/releaseway --version <version> \
  --set bucket.endpoint=https://<account id>.r2.cloudflarestorage.com \
  --set bucket.name=<bucket> --set existingSecret=releaseway
```

`chart/releaseway/values.yaml` has the rest: the hosts, an Ingress, the
scratch volume's size, resources. Or `secrets.*` instead of
`existingSecret`, for a Secret the chart makes.

## Developing

```sh
cargo test    # a stand-in bucket and a stand-in GitHub, nothing over the network
cargo run     # with the variables above
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy and the tests, and lints
the chart. Every merge to main is a patch release: a `vX.Y.Z` tag, the
image for amd64 and arm64, and the chart. A minor or major version is a tag
pushed by hand, and the merges after it count on from there.

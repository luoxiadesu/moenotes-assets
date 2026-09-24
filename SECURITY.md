# Security Policy

This alpha has no authentication. Bind to loopback or use a trusted network and
an authenticated, rate-limited reverse proxy. Any caller can consume CPU, network
bandwidth and storage. There is no claim of safe anonymous Internet deployment.

Keep the data directory private to the service UID. Database and filesystem
contents are trusted local state; do not allow other users to replace indexed
files, SQLite files, or working directories while the service runs. Configure a
container memory limit, filesystem quota and PID limit as additional boundaries.
Worker resource limits and separate processes are not a security sandbox.

The worker and media-exec commands are internal process interfaces, not exposed
through HTTP. Configure CDN roots and executable paths only from trusted sources.
Downloads do not follow redirects or inherit proxy variables. Export names are
service-generated; asset labels are untrusted metadata, not filesystem paths.

Malformed assets can still expose defects in third-party parsers or native codecs.
Keep dependencies and the runtime image patched. No complete dependency-security
audit or comprehensive fuzzing campaign is claimed for this prerelease.

Please report vulnerabilities privately using this repository's GitHub security
advisories. Do not include player credentials or copyrighted asset payloads in
public issues. Describe versions and reproduction steps using synthetic fixtures
where possible. Only the latest prerelease is maintained at present.

Tree outputs are trusted operator-controlled state. Do not mutate them while
exporting/uploading. The service has no local dependency provider. This is not a
sandbox against a malicious local user racing indexed filesystem changes. Diagnostic files may contain private input paths from
external tools; do not expose the diagnostics directory. S3 credentials are read
only from environment variables and are never written into the public tree.

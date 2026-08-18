# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability within Kern, please send an email to the
maintainers via GitHub. All security vulnerabilities will be promptly addressed.

Please do not report security vulnerabilities through public GitHub issues.

## Disclosure Policy

When the maintainers receive a security bug report, they will confirm the vulnerability
and determine its impact. The maintainers will:

1. Confirm the vulnerability and determine the affected versions
2. Audit for any similar issues
3. Prepare a fix and release it as soon as possible
4. Publish a security advisory once the fix is released

## Security Best Practices

When using Kern in production:

- Keep the API token (`$KERN_HOME/token`) confidential
- Bind the API to loopback only (default: `127.0.0.1:8787`)
- Use `sandbox: required` for agents running untrusted code
- Set appropriate permission policies (deny by default)
- Enable redaction logging in production environments
- Monitor agent events for unexpected behavior

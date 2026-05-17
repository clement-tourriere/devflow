# Security Policy

## Supported Versions

| Version | Supported |
|---|---|
| 0.3.x | ✅ Current |
| < 0.3.0 | ❌ |

## Reporting a Vulnerability

If you discover a security vulnerability in devflow, please report it responsibly:

1. **Do not** open a public GitHub issue
2. **Email**: clement.tourriere@gmail.com with subject "devflow security"
3. Include:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
   - Suggested fix (if any)

You should receive an acknowledgment within 48 hours. We aim to release a fix within 7 days of confirmed vulnerabilities.

## Security Considerations

### Credentials in Configuration

- `.devflow.yml` may contain API keys for cloud providers (Neon, DBLab, Xata)
- Use `.devflow.local.yml` (gitignored) for secrets, or environment variables
- Never commit credentials to version control

### Hook System

- Hooks execute shell commands defined in `.devflow.yml`
- The approval system prevents unauthorized hook execution in `--non-interactive` mode
- Review hook commands before approving: `devflow hook show`
- devflow does not provide a general-purpose workspace sandbox; use your agent harness, containers, VMs, or a dedicated sandbox platform for hard isolation

### Proxy Certificates

- `devflow proxy` generates a local CA certificate
- The CA private key is stored in `~/.local/share/devflow/proxy/`
- `devflow proxy trust install` adds the CA to your system trust store
- Remove with `devflow proxy trust remove` when no longer needed

### Docker Access

- devflow requires Docker socket access for local providers
- Containers run with default Docker security settings
- Service passwords default to development-safe values (not for production use)

## Dependencies

We run `cargo audit` in CI to catch known vulnerabilities in dependencies. You can run it locally:

```bash
cargo install cargo-audit
cargo audit
```

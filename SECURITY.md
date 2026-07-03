# Security Policy

## Supported versions

WinSearch is under active development; security fixes target the latest release
and `main`.

## Reporting a vulnerability

Please report suspected vulnerabilities privately rather than opening a public
issue. Use GitHub's [private vulnerability reporting](https://github.com/STE-FalconSoftware/WinSearch/security/advisories/new)
for this repository, or email **security@ste-falcon-software.com**.

Include the affected version/commit, reproduction steps, and impact. We aim to
acknowledge reports within a few business days.

## Scope notes

WinSearch requires administrator rights to read the NTFS Master File Table and
USN change journal — this is inherent to the fast indexing approach, not a
privilege-escalation bug. The unprivileged `--root` folder mode is available for
environments where elevation is undesirable.

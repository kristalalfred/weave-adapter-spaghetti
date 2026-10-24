---
id: SP-1
title: "Installed spaghetti nodes lose the control plane when the bench Mac's LAN address changes"
type: bug
status: todo
depends_on: []
assignee:
---

## Evidence

`just installer` substitutes the bench Mac's current LAN address
into `install.sh` as `southbound_url` (`http://<lan>:29081`), and the installer
writes it into every node's config. On 2026-10-23 the bench Mac moved from
192.168.3.121 to 10.4.0.86 overnight. Both installed nodes kept dialling
`http://192.168.3.121:29081`, failed every sync with a connect error, and went
offline. Their observed sites were otherwise correct. Re-serving the installer
and re-running the curl command on both Macs restored them.

The bench Mac's own node has the same problem, although southbound runs on that
Mac.

## Done when

- [ ] A node keeps reaching southbound after the bench Mac's LAN address
  changes, without reinstalling.
- [ ] The bench Mac's own node reaches southbound without going through its LAN
  address.

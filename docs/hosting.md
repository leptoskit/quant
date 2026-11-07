# Reaching a vault from anywhere

Quant's promise is that only the owner can read their vault. Every way of reaching it from outside the home network has to keep that promise against the operator of whatever server sits in the middle, not just against strangers. This page sets out the options and which ones Quant builds.

## The question that decides everything: who can use the key?

Whatever answers an agent's question has to decrypt the vault to do it. So "can agents reach it from anywhere, unattended" and "can the operator read it" are the same question asked twice: the machine that answers holds the key while it answers. Encryption at rest does not change that; it only decides who holds the key the rest of the time.

| Design | Unattended, from anywhere | Operator can read it |
| --- | --- | --- |
| Vault on your machine, reached through a blind relay | Yes, while your machine is on | No: the relay sees only TLS |
| Hosted, keys sealed by a server key or a cloud KMS | Yes | Yes, at any time |
| Hosted, keys sealed by the owner's passphrase or passkey | Only while the owner has unlocked it | Yes, while unlocked |
| Hosted in an attested enclave, keys released only to verified code | Yes | No, if the enclave and the build are sound |

## 1. The relay (built first)

The vault stays on the owner's machine. The daemon keeps an outbound connection to a relay; a phone, a sensor bridge, or an MCP server on another computer connects to the relay; the relay joins the two connections and copies bytes. TLS runs end to end, from the client to the daemon, inside that pipe, and clients pin the daemon's certificate fingerprint from the pairing link exactly as they do on the local network. Requests inside are signed by the client's own key, as everywhere else.

- The relay knows a rendezvous id (a hash of a key the daemon derives from the vault key), the IP addresses that connect, when, and how many bytes flow. It never sees a request, a response, a key, or a pairing code.
- A relay that lies can drop or delay traffic, but it cannot read or alter it: an altered TLS record fails, and a fake daemon fails the pinned fingerprint.
- The daemon proves it owns its rendezvous id by signing the relay's challenge, so nobody else can take its place on the relay.
- Phones keep their upload spool while the daemon is unreachable, so the relay needs no mailbox.
- Limits per rendezvous id (connections, bytes per day, idle time) keep one vault from starving the rest.

The relay is a small leptoskit server: `leptoskit_db` keeps registrations and usage counters, and `leptoskit generate deploy` writes the Docker, Fly, Compose, Kubernetes, or systemd files. Because it never holds a key, anyone can run one, and `leptoskit verify` lets users check that a public relay runs the published code.

## 2. Hosted vaults (designed, not built)

For people who want agents to reach their vault while their own machines are off, a hosted service would keep each vault as its own encrypted SQLCipher file and media folder in object storage, and open it in a per-vault worker process when a request arrives. The central `leptoskit_db` database would hold accounts, routing, billing, and each vault's **wrapped** key, never life data. The worker runs the same code as the daemon, so grants, rate limits, redaction, and the hash-chained audit log still apply inside it.

What protects the key is the whole design:

- **Never a server-held key.** A key the server can unwrap on its own makes every vault readable by the operator, by anyone who compromises the server, and by anyone who can compel either. That is ordinary SaaS encryption at rest, and it protects only against stolen disks and backups. Quant will not offer it.
- **Unlock windows (interim).** The key is wrapped under the owner's passphrase or passkey. The owner unlocks the vault for a window ("the next 8 hours", from the phone with a passkey), the worker holds the key in memory until the window ends, and agents work only during it. Outside the window, nothing on the server can open the vault. Inside it, a malicious operator who changes the running code could read it, so this protects against breaches of storage and the database, not against the operator.
- **Attested enclaves (the goal).** The worker runs in a confidential-computing enclave (AWS Nitro Enclaves, AMD SEV-SNP, or Intel TDX). Each vault key is wrapped under a key-management policy that releases it only to an enclave whose measured code matches a published, reproducible build, which is what `leptoskit verify` already checks for backends. The operator runs the hardware but cannot read the enclave's memory or swap in other code without the key being withheld. This is the model Apple uses for Private Cloud Compute, and it is the only hosted design in which "from anywhere, unattended" and "the operator cannot read it" are both true.

Either hosted mode adds a fourth thing to trust beyond the owner's own devices: the enclave vendor in the second, the operator in the first. The relay asks for no such trust, which is why it comes first.

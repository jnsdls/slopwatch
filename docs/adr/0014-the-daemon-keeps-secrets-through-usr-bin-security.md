# The daemon keeps Secrets through `/usr/bin/security`

A Secret's value is a generic password item in the developer's login Keychain. The service is `slopwatch`, or `slopwatch-dev` for dev builds, and the account is the Secret's name. The daemon never calls the Security framework for these items. It runs `/usr/bin/security` for every write, read and delete, and each item lists `/usr/bin/security` as its trusted app (`-T`). The SQLite store records only which names are set and when.

The reason is the ad-hoc signature (ADR 0009, "What the first build showed"). The Keychain ties an item's access list and partition list to the code that created it, and an ad-hoc identity is the binary's cdhash. Items the daemon created itself would prompt after every rebuild, and a launchd agent has nobody watching for the prompt. `/usr/bin/security` is Apple-signed and the same binary on every build, so a rebuilt daemon reads its items through it with no prompt.

The value never goes in argv, where `ps` shows it. The daemon starts `security -i`, writes one `add-generic-password -U ... -X <hex>` command on its stdin, and reads values back from `find-generic-password -w` on a pipe. Every call has a 30 s timeout. A locked Keychain makes `security` wait for the developer's password, and the daemon gives up instead of hanging. A Step whose required Secret can't be read then errors `error(secret unreadable)`.

## Considered options

- Create the items with the Security framework from the daemon. That is the obvious API, and it prompts after every rebuild until builds carry a Team ID.
- The data protection Keychain (`kSecUseDataProtectionKeychain`). It keys access on a keychain access group, which needs an entitlement backed by a provisioning profile, so an ad-hoc build can't use it at all.
- An access list that admits any application (`security add-generic-password -A`). It drops the one check the Keychain still makes, and I didn't try whether the partition list would still prompt a rebuilt daemon.
- An encrypted file in the data dir. Its key would need a home, which is the same problem again.

## Consequences

- Any process running as the developer can read the items with `security find-generic-password -s slopwatch -a <name> -w`, without a prompt. That's the same exposure the developer's own `-T /usr/bin/security` items have, and the same reach an approved Plugin already has, since Steps run unsandboxed as the developer (ADR 0003). The Keychain still encrypts the items at rest and locks them with the login Keychain.
- A free Apple Development identity (ADR 0009) doesn't change this decision. A daemon with a Team ID could own its items through the framework, but `/usr/bin/security` keeps working across that switch, so nothing has to migrate.
- Values are cached in the daemon's memory once read or set, and the daemon reads every set value once at start. A Step spawn doesn't wait on `security`, and rotating goes through the daemon, which replaces the cached value. A value changed in Keychain Access by hand reaches Steps only after the daemon restarts.
- The daemon takes values of at least 8 characters with no control characters, trimmed of surrounding whitespace. A shorter value would mask common words out of every log. A multi-line value, such as a PEM key, isn't supported in v1.
- Masking covers each value and its JSON-escaped form, in Step log records, protocol messages from the Step (Outcome, progress, Effect requests) and protocol errors. Values split across the pieces of a stderr line longer than a log record are masked too. Encoded forms, such as a value in base64, aren't.

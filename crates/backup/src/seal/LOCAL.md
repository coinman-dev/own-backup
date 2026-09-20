# Local service authorization (E2)

Implemented: account-scope DPAPI, private DACL, immutable local key files,
transactional job persistence, and a fail-closed queue guard. This does not
enable encrypted image creation yet.

## Production boundary

`ServiceKeyStore::new(data_dir)` is Windows-only and refuses WinPE.
The process must run as LocalSystem, with no thread impersonation token.
Both conditions are checked on every operation. There is no LocalMachine
DPAPI flag, saved-password fallback or Linux plaintext/key-file fallback.

The directory is `data_dir/.own-backup-service-keys`. Directory and files
are created with SYSTEM ownership and a protected DACL granting only SYSTEM
full access. Existing objects must already satisfy that policy: they are
not silently adopted by changing their ACL. Handles validate the owner,
protected DACL, single allowed ACE, object type and absence of reparse points.
Files with additional hard links are refused. Directory handles remain open
without delete sharing during store operations.

This does not isolate keys from other SYSTEM processes or administrators.
DPAPI-allocated plaintext is wiped before LocalFree; working key buffers use
Zeroizing. The local store contains private keys, never user passwords.

## Local file and binding

Each authorization gets a fresh UUID. Its filename is `<uuid>.dpapi`;
neither a raw job ID nor a caller-supplied path becomes a filesystem component.
`LocalKeyRef` serializes only that UUID.

A file is at most 16 KiB: prefix `OBKDP\0\x01\0` (8 bytes), followed by
DPAPI-protected 32-byte private X25519 key material.
Protect/unprotect use `CRYPTPROTECT_UI_FORBIDDEN`. Additional entropy is:

```text
UTF8("own-backup/v2/local-service-key/1")
|| authorization_uuid[16]
|| job_id_utf8_length_u16_be || UTF8(job_id)
|| full_public_job_key[32]
```

This binds version, authorization, job and recipient. It is context binding,
not another secret or a replacement for ACLs. The reopened private key must
reproduce the expected public key.

The portable password/recovery document has no DPAPI slot. A serialized job
contains the portable envelope and a local reference, never the local blob.

## Transaction and job guard

The caller must have created/unlocked the portable envelope, confirmed recovery
delivery, and serialized key mutations against running/queued jobs before the
wizard/RPC is connected in E5.

1. Validate portable metadata and the private/public pair.
2. Create a new protected file exclusively and flush it.
3. Reopen the actual file and verify the key.
4. Invoke the job-commit callback only after that succeeds.
5. On callback failure remove only the new authorization. The old reference
   and portable password/recovery access remain intact.

The callback must publish nothing on error.
It must not re-enter the same store; revoke old references after the outer
transaction returns.
`JobStore::upsert_with_archive_key` uses the existing disk-first job write.
A crash before commit can leave an unreferenced protected file, never a job
enabled before its key was saved. Reauthorization uses a new UUID, not an
overwrite. Explicit revocation must follow durable removal/replacement of
the reference. Old/orphaned authorization collection is not yet wired.

Ordinary SaveJob cannot introduce or replace authorization. Older clients
omitting encryption while editing another setting retain the saved encryption.
Switching an encrypted job to VHDX is refused.

The queue reopens authorization on each encrypted run. Missing, damaged or
inaccessible keys give a reauthorization error without a prompt.
Until E3/E4, even a valid key stops before the plaintext writer with an explicit
implementation-unavailable error.

## Remaining integration before enabling encryption

- Mandatory exclusion of the local directory from applicable backup sources,
  including user-disabled exclusions. Raw/sector copies and scanner failures
  need fail-closed handling. A removable default pattern is insufficient.
  This is an E4 integration gate.
- Thread keys through encrypted records, parent reads, scratch and maintenance.
- Connect dedicated service setup/rebinding after recovery confirmation, with
  operation/queue serialization (E5).
- Retire local references after successful job deletion/rebinding, reporting
  cleanup failures without damaging portable slots.
- Verify real SCM/LocalSystem access after an OS restart on a test VM.

No GUI option or setup RPC has been enabled prematurely.

## Tests and disposable SCM probe

Portable tests inject an authenticated fake OS backend and cover failed writes,
failed reopen, failed job commit, reference/blob swaps, old-key preservation
and loss of the local OS key. Native Windows tests run real DPAPI/DACL code
under the disposable test account; production construction requires LocalSystem.
The native tests cover reopening, inherited ACLs, hard links, size limits,
ordinary-account rejection and same-user thread impersonation.
These tests do not establish the SCM cold-start result.

Build the independent service probe:

```text
cargo xwin build --locked --offline --target x86_64-pc-windows-msvc -p own-backup-service --example seal_store_service
```

Inside a disposable Windows VM, in Administrator PowerShell:

```powershell
./scripts/test-seal-service.ps1 -Action Install -BinaryPath C:/Lab/seal_store_service.exe
# Shut down/start the VM; wait for automatic service startup before login.
./scripts/test-seal-service.ps1 -Action Verify
./scripts/test-seal-service.ps1 -Action Remove
```

The script registers only automatic LocalSystem service `OwnBackupSealProbe`.
It refuses an existing service/directory instead of overwriting it.
The isolated fixture is under `ProgramData/OwnBackup-SealProbe-Lab`.
First start creates a random password/recovery key, saves the authorized job
and discards the secrets. Subsequent starts reopen through DPAPI alone.
It never runs a backup, mount, restore or disk operation.

Verify requires a changed OS boot time and a successful post-boot report.
Remove unregisters the probe service and leaves evidence/fixtures.
The script does not initiate a reboot.

The unsigned host test executable triggered Avast CyberCapture; the screenshot
showed pending analysis, not a final verdict. Native reruns on this host were
paused. No antivirus settings/exclusions were changed. The laboratory service
has been built but not installed.

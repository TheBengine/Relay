# Relay native frame cryptor patch

Apache-2.0 upstream copyright and license notice are retained in both source files. The base SDK LICENSE.md is retained in every prepared SDK.

This patch replaces only the existing frame cryptor library object. Headers and public ABI stay unchanged. Media frames use independent random 96-bit IVs, with a saturating process-wide limit of 2^32 attempts and refusal on RNG failure. Two key-material diagnostic messages are removed. DataPacketCryptor is unchanged; Relay chat and files use their own MLS/AEAD protocols.

Use scripts/prepare-webrtc.ps1 after the original digest-checked SDK fetch. It verifies the base files, compiles the patch with the static MSVC runtime, replaces exactly one archive member in a new owned directory and records library/source/compiler hashes. Neither the downloaded ZIP nor the verified original SDK is modified. The native client build checks the prepared marker and library digest.

The original reproduction is in docs/native-iv-reproduction-20261006.json. Integrated verification is in docs/verification-native-frame-crypto-0.10.0.json; task 29 is complete. Generated frames do not prove physical video or WAN performance. This README completion note was updated after the recorded 592-input build; executable inputs are unchanged.

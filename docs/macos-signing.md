# macOS signing and notarization

`.github/workflows/release.yml` signs the macOS bundle with a Developer ID
certificate and notarizes it when the repository secrets below are set. When
they are missing (forks, local builds), `scripts/package-macos.sh` falls back
to an ad-hoc signature.

| Secret | Required for | Value |
| --- | --- | --- |
| `MACOS_CERTIFICATE_P12` | signing | `base64 -i DeveloperID.p12` of the exported "Developer ID Application" certificate and private key |
| `MACOS_CERTIFICATE_PASSWORD` | signing | password chosen when exporting the `.p12` |
| `MACOS_CODESIGN_IDENTITY` | optional, variable or secret | e.g. `Developer ID Application: Name (TEAMID)`; detected from the certificate when unset |
| `APPLE_API_KEY` | notarization | `base64 -i AuthKey_XXXXXXXXXX.p8` of an App Store Connect API key |
| `APPLE_API_KEY_ID` | notarization, variable or secret | the key ID (`XXXXXXXXXX`) |
| `APPLE_API_ISSUER` | notarization, variable or secret | the issuer ID shown on the App Store Connect "Keys" page |

Local builds use the same environment variables: `CODESIGN_IDENTITY`,
optional `CODESIGN_KEYCHAIN`, and `NOTARY_API_KEY_PATH`,
`NOTARY_API_KEY_ID`, `NOTARY_API_ISSUER`.

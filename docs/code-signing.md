# Windows Code Signing

Turso's Windows build artifacts are Authenticode-signed with [Azure Artifact
Signing](https://azure.microsoft.com/en-us/products/artifact-signing) (formerly
**Azure Trusted Signing**). Signing is what lets Windows SmartScreen and the OS
verify that a binary genuinely comes from Turso and hasn't been tampered with.

The .NET publishing workflow signs on the Windows runners where the binaries are already built:

| Workflow | Artifact signed |
|----------|-----------------|
| `.github/workflows/dotnet-publish.yml` | `turso_sdk_kit.dll` (win-x64, win-arm64) inside the NuGet package |

Signing is a no-op unless the `AZURE_SIGNING_ENABLED` repository **variable** is
`true`, so forks and pull requests without access to the secrets build normally
without signing.

CLI releases publish a Docker image instead of standalone Windows binaries.

## GitHub Actions configuration

The .NET workflow authenticates to Azure with [OpenID Connect (OIDC)](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/configuring-openid-connect-in-azure),
so there is no long-lived certificate or client secret stored in the repo.

### Repository secrets

Configure under **Settings → Secrets and variables → Actions → Secrets**
(`gh secret set <NAME>`):

| Secret | Description |
|--------|-------------|
| `AZURE_CLIENT_ID` | Application (client) ID of the Entra app registration used for signing |
| `AZURE_TENANT_ID` | Microsoft Entra (Azure AD) tenant ID |
| `AZURE_SUBSCRIPTION_ID` | Azure subscription ID containing the signing account |

### Repository variables

Configure under **Settings → Secrets and variables → Actions → Variables**
(`gh variable set <NAME> --body <VALUE>`):

| Variable | Description | Example |
|----------|-------------|---------|
| `AZURE_SIGNING_ENABLED` | Master switch; signing runs only when this is `true` | `true` |
| `AZURE_SIGNING_ENDPOINT` | Signing account endpoint for your region | `https://eus.codesigning.azure.net` |
| `AZURE_SIGNING_ACCOUNT` | Artifact Signing account name | `turso-signing` |
| `AZURE_SIGNING_CERT_PROFILE` | Certificate profile name | `turso-public-trust` |

These are non-secret, so variables (not secrets) are intentional — it keeps them
visible and easy to audit.

### Example: setting everything with the `gh` CLI

```bash
gh secret set AZURE_CLIENT_ID --body "<app-client-id>"
gh secret set AZURE_TENANT_ID --body "<tenant-id>"
gh secret set AZURE_SUBSCRIPTION_ID --body "<subscription-id>"

gh variable set AZURE_SIGNING_ENABLED --body "true"
gh variable set AZURE_SIGNING_ENDPOINT --body "https://eus.codesigning.azure.net"
gh variable set AZURE_SIGNING_ACCOUNT --body "<signing-account-name>"
gh variable set AZURE_SIGNING_CERT_PROFILE --body "<certificate-profile-name>"
```

## One-time Azure setup

This is done in the Azure portal / Entra and only needs to happen once. See the
[Artifact Signing quickstart](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart)
for full detail.

1. **Paid subscription.** Artifact Signing does not work on free, trial, or
   sponsored subscriptions.
2. **Create the signing account and a Public Trust certificate profile.** This
   requires a completed *identity validation* request. The legal name and
   address on your Azure billing account must exactly match what you want on the
   certificate, and the billing account type (Individual vs Organization) must
   match the validation type.
3. **Register an Entra app for CI** and add a **federated credential** for GitHub
   OIDC. Classic federated credentials match the subject **exactly** — a
   literal `*` in the subject never wildcard-matches, so a credential like
   `refs/heads/*` silently fails with `AADSTS7002131`. The subject must
   match the ref the workflow signs on: `dotnet-publish.yml` signs only on
   **main pushes** (and manual dispatch from main) →
   `repo:tursodatabase/turso:ref:refs/heads/main`. Add a federated credential
   for this ref.
   See [Configuring OpenID Connect in Azure](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/configuring-openid-connect-in-azure).
4. **Assign roles** on the signing account to that app:
   - **Trusted Signing Certificate Profile Signer** — required for the app to
     request signing certificates.
   - You also need **Trusted Signing Identity Verifier** to complete step 2.

   See [Artifact Signing resources and roles](https://learn.microsoft.com/en-us/azure/artifact-signing/concept-resources-roles).

## Verifying a signature

Download the .NET package and check its DLL on Windows:

```powershell
signtool verify /pa /v turso_sdk_kit.dll
```

Or inspect the **Digital Signatures** tab in the file's Properties dialog.

## Reference

- [Azure Artifact Signing product page](https://azure.microsoft.com/en-us/products/artifact-signing)
- [Artifact Signing quickstart](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart)
- [Artifact Signing resources and roles](https://learn.microsoft.com/en-us/azure/artifact-signing/concept-resources-roles)
- [Artifact Signing FAQ](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)
- [`azure/trusted-signing-action`](https://github.com/Azure/trusted-signing-action) (the GitHub Action; also published as [`azure/artifact-signing-action`](https://github.com/Azure/artifact-signing-action))
- [Configuring OpenID Connect in Azure](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/configuring-openid-connect-in-azure)

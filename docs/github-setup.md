# GitHub setup

PRowl acts on GitHub **as you**: it reads the pull requests you're asked to review and posts
reviews and comments under your account. There are two ways to give it access.

| | OAuth App (recommended) | Personal access token |
|---|---|---|
| Sign-in | One-time code in the browser | Paste a token |
| Sees | Every repository you can access — personal, collaborator and organisation | Whatever the token allows |
| Org admin needed? | No, unless the organisation restricts third-party OAuth apps | No (classic tokens), or org approval for fine-grained tokens if the org requires it |

> **Why not a GitHub App?** A GitHub App only sees repositories where it has been *installed*,
> which for organisation repositories needs an org owner. An OAuth App works with your own
> access, so you can use PRowl on every repository you can review without asking anyone.

## Option 1 — OAuth App (device flow)

1. Open **GitHub → Settings → Developer settings → OAuth Apps → New OAuth App**
   (<https://github.com/settings/applications/new>).
2. Fill in:
   - **Application name:** `PRowl` (anything you like)
   - **Homepage URL:** `https://github.com/AmirhosseinBahrani/prowl`
   - **Authorization callback URL:** `http://127.0.0.1` (not used by the device flow, but required)
3. Create it, then tick **Enable Device Flow** and save.
4. Copy the **Client ID** (it starts with `Ov23`). No client secret is needed.
5. Give it to the daemon. The simplest way, which the background service also sees, is a file
   in the PRowl home ([where is that?](configuration.md#files)):
   ```sh
   echo Ov23li... > "<prowl home>/github-client-id"
   ```
   PRowl reads the file each time you start a login, so no restart is needed.
   Alternatively set `PROWL_GITHUB_CLIENT_ID` in the daemon's environment. Release builds may
   have the project's client id built in; the environment variable and the file both override it.
6. Run `prowl`, choose **Connect GitHub**, and enter the code shown (press `o` to open the page,
   or scan the QR code with your phone).

PRowl requests the `repo` and `read:org` scopes: `repo` to read private pull requests and post
reviews, `read:org` to see organisation membership and team review requests.

### Organisations that restrict OAuth apps

If an organisation has **third-party application access restrictions** enabled, its repositories
won't appear until the app is approved. On <https://github.com/settings/connections/applications>,
open your PRowl app and click **Request** (or **Grant**, if you're an owner) next to the
organisation. Alternatively, use a personal access token.

## Option 2 — Personal access token

Useful when an organisation blocks OAuth apps, on headless servers, or if you'd rather not
register an app.

- **Classic token** (<https://github.com/settings/tokens/new>): select `repo` and `read:org`.
  For organisations using SAML single sign-on, click **Configure SSO → Authorize** next to the
  token.
- **Fine-grained token** (<https://github.com/settings/personal-access-tokens/new>): choose the
  resource owner (you or an organisation) and the repositories, then grant
  **Pull requests: Read and write**, **Contents: Read**, **Issues: Read and write** (labels and
  comments) and **Metadata: Read**. Organisations may require approval of fine-grained tokens.

Then sign in with it — in the TUI's **Connect GitHub** screen press `t` and paste the token, or
from a shell:

```sh
prowld login --token            # prompts for the token without echoing it
echo "$GITHUB_TOKEN" | prowld login --token
```

PRowl checks the token against GitHub before storing it in your OS keychain.

## Disconnecting

`prowld logout`, or **Disconnect** in the TUI, deletes the stored token. To revoke access on
GitHub's side as well, remove the app at <https://github.com/settings/applications> or delete the
token.

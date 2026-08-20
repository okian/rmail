# Provider settings

One row per provider people actually have: the two hostnames, the two ports,
whether a plain password is accepted at all, and where the thing you have to
fetch yourself is kept. [[add-any-account]] is the procedure these values go
into.

Try the discovery before reading any of it:

```
mail account add you@example.com
```

Every row below is what a correct probe returns for that provider. The page is
for the case where the probe missed, where you want to check its answer, or
where the answer was settings and the question was really "and what password".

## What rmail requires, of all of them

- IMAP is TLS from the first byte. The client only ever opens a TLS socket, so
  993 in practice. A provider offering STARTTLS on 143 is not refused by the
  discovery — it comes back with a warning that it will not sync as it stands
  — but it will fail at connect time. Only an unencrypted socket is refused
  outright, and that one the settings type cannot express.
- SMTP is decided from the port: send.smtp_security defaults to auto, which
  means implicit TLS on 465 and STARTTLS everywhere else. Pinning it takes one
  of auto, starttls, implicit_tls, or plaintext for a local relay you trust —
  the one setting in rmail that will put credentials on the wire, and it has
  to be asked for by name.
- Ports default to 993 and 587 when you omit them.
- The trust store is the public one, with no per-account root to add. A
  certificate signed by your own CA cannot be accepted however correct the
  rest of the configuration is. Three rows below turn on this.
- The username is the full address unless the row says otherwise.

## Gmail and Google Workspace

```
imap.gmail.com:993
smtp.gmail.com:587
```

- Login: either. An app password works on a personal account with 2-Step
  Verification switched on; what Google retired was the account password
  itself. OAuth is the path that is not being narrowed, and the only one a
  Workspace administrator cannot switch off underneath you.
- App passwords live under your Google Account, Security, 2-Step
  Verification, App passwords. The page does not exist until 2-Step
  Verification does.
- To register an OAuth client: console.cloud.google.com, under Google Auth
  Platform, Clients, as a client of type Desktop app. Google issues that type
  a client secret and requires it, which is what --client-secret-command is
  for.
- Set that project's publishing status to In production before you rely on it.
  While it is Testing, Google expires every refresh token after seven days,
  and what you see is an account that syncs for a week and then stops — which
  reads exactly like a token that expired while the machine was asleep.
- The scope is the single coarse https://mail.google.com/. The finer
  gmail.readonly and gmail.modify reach the REST API only, and an IMAP
  AUTHENTICATE XOAUTH2 carrying one of them fails.
- IMAP is on for every mailbox and there is no per-user switch any more. A
  Workspace administrator can still turn it off per organisational unit in the
  Admin console, which is the only place to check when a Workspace account
  cannot connect.
- All Mail is the archive folder here, which is why {{keys:message.archive}}
  finds one — see [[archive]].

## Microsoft 365 and Outlook.com

```
outlook.office365.com:993
smtp.office365.com:587
```

- Login: OAuth, and only OAuth. Basic authentication for IMAP and SMTP is
  retired across Exchange Online and the consumer service, so a password that
  works in a browser is refused here however it is stored.
- Register the client at the Microsoft Entra admin centre, App registrations,
  as a Mobile and desktop application with a loopback redirect. It is a public
  client and needs no secret.
- Two tenant-side switches stop a correct client dead. SMTP AUTH is disabled
  by default in organisations created since January 2020 and by Security
  Defaults, and IMAP can be turned off per mailbox. The symptom is mail that
  arrives and will not send, or an account that authenticates for neither; the
  fix is in Exchange admin rather than in rmail.
- The default scopes are offline_access — without it Microsoft issues no
  refresh token and the grant dies with the access token — plus
  IMAP.AccessAsUser.All and SMTP.Send under outlook.office.com.
- The common tenant is used, which covers work-or-school and personal accounts
  alike, because a client cannot know which kind it has been pointed at.
- A personal outlook.com, hotmail.com or live.com mailbox sends through
  smtp-mail.outlook.com rather than smtp.office365.com. Autodiscover reports
  the right one; the pair above is what a tenant gets.

## iCloud

```
imap.mail.me.com:993
smtp.mail.me.com:587
```

- Login: an app-specific password. Generate it at account.apple.com, under
  Sign-In and Security, App-Specific Passwords. It is shown once — put it
  straight into the keychain or behind a password command.
- An Apple Account with two-factor authentication switched on has no other
  option: the account password is refused. Legacy accounts that were never
  enrolled can still use theirs, and have no App-Specific Passwords page at
  all.
- An iCloud+ custom domain uses these same two hosts, but the username is the
  Apple Account's own icloud.com address rather than the custom one. This is
  the row where the rule above does not hold, and getting it wrong looks like
  a bad app password.

## Yahoo and AOL

```
imap.mail.yahoo.com:993
smtp.mail.yahoo.com:465
```

- Login: an app password, from Yahoo's Account Security page. The account
  password is refused.
- AOL runs on the same backend at imap.aol.com and smtp.aol.com, on the same
  ports — but an AOL mailbox's app password is generated on AOL's own security
  page, not Yahoo's. The accounts are not interchangeable.
- 465 is the interesting part of this row. With send.smtp_security left at
  auto, that port selects implicit TLS on its own, because STARTTLS on 465
  would hang waiting for a greeting that is already encrypted. Nothing to
  configure — which is why the port is not a detail.

## Fastmail

```
imap.fastmail.com:993
smtp.fastmail.com:587
```

- Login: an app password, created under Settings, Privacy and Security,
  Integrations. Scope it to Mail rather than to everything, so a leaked
  credential cannot read your calendar or your files.
- 465 works too and takes implicit TLS, which the port selects on its own.
  Pick one and let the default follow it.
- Fastmail publishes a correct autoconfig document, so mail account add
  answers this row without being told anything.

## Zoho

```
imappro.zoho.com:993
smtppro.zoho.com:465
```

- Login: an app password when two-factor authentication is on, from Zoho's own
  security page.
- The hosts above are the paid and organisation ones. IMAP is not included in
  the Forever Free plan at all, so a free mailbox has nothing to configure
  here; imap.zoho.com is the free-plan host and will not help.
- Both are per region, matching the data centre the account was created in:
  imappro.zoho.eu, imappro.zoho.in, imappro.zoho.com.au. The wrong region
  fails to log in rather than failing to connect, which reads as a wrong
  password. The discovery gets this right; guessing does not.

## A server you run: Dovecot, Postfix, or a mail-in-a-box

```
imap.example.com:993
smtp.example.com:587
```

- Login: whatever you configured. A password command against your own password
  manager is the obvious fit.
- This is the row the public trust store bites on: a self-signed or private-CA
  certificate fails the handshake and there is no override. Get a
  publicly-trusted certificate for the host.
- Name your archive folder Archive, Archives or All Mail. Those three are
  compiled into the client rather than configured, so a folder called
  Filed is one {{keys:message.archive}} cannot find — see [[archive]].
- Publish an autoconfig document at autoconfig.example.com and every client
  including this one stops needing to be told. It is a static XML file.

## Shared hosting and cPanel

```
mail.example.com:993
mail.example.com:465
```

- Login: the mailbox password from the hosting control panel, which is usually
  also where the two hostnames are printed.
- Read that page carefully: cPanel prints mail.example.com under the
  non-SSL settings and the *server* hostname under the secure ones, because
  the certificate frequently does not cover the mail subdomain. Use whichever
  name the certificate is for, or the handshake fails.
- These hosts rarely publish autoconfig or SRV records, so this is the common
  case for typing the block yourself. mail account add --ai will propose one,
  but it is never login-checked — see [[add-any-account]] — so the check
  afterwards is the whole of the verification.

## Proton Mail

Not usable today, and the reason is worth stating rather than leaving to be
discovered. Proton exposes no IMAP of its own; the Bridge does, on loopback,
and only on a paid plan. Its listeners can be set to implicit TLS, so that
half is configurable — but the certificate it presents is signed by a root
generated on your own machine at install time, and there is no per-account
root to add. That is the one blocker, and no combination of settings gets
around it.

## When none of the above is your provider

Four places to ask, which are the four the discovery asks:

- The domain itself, at autoconfig.example.com/mail/config-v1.1.xml.
- Mozilla's ISPDB, which carries thousands of providers this page does not. A
  provider in it needs no research at all.
- Microsoft autodiscover, for anything hosted on a Microsoft tenant.
- DNS, where RFC 6186 puts the answer in _imaps._tcp and _submission._tcp SRV
  records.

Failing all four, the provider's own help pages under the phrase "IMAP
settings". What you are looking for is four values — two hostnames and two
ports — plus the answer to the only question that really varies, which is
whether they will accept a password at all.

## Where to read next

- [[add-any-account]] — the five steps these values go into.
- [[accounts-and-tokens]] — the account verbs from inside the client.
- [[add-oauth-account]] — the OAuth half in detail, with Gmail as the example.
- [[config-file]] — where send.smtp_security and the per-account policy live.
- [[troubleshooting]] — when the login does not come back.

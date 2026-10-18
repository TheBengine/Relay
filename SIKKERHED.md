# Sikkerhed i Relay — kort fortalt

## Hvem kan læse indholdet?

**I beskyttet tilstand** krypterer Relay indholdet på deltagernes enheder. De godkendte enheder har nøglerne; Relay-serveren videresender det krypterede indhold. Deltagerne kan stadig gemme og dele det, de modtager.

**I åben tilstand** kan serveren læse indholdet af beskeder, filer og opkald, som deltagerne accepterer at sende uden end-to-end-kryptering. En ændring annonceres tydeligt; hver deltager skal selv acceptere den. Hvis serveren senere skifter til åben tilstand igen, kræver det ny accept. Ved serveroprettelse er end-to-end-kryptering anbefalet og forvalgt.

## Hvad betyder invitationen?

En ny engangsinvitation kan bekræfte de to enheder automatisk. Send den kun gennem en kontaktvej, du stoler på, og til den rigtige ven. Et lækket, ubrugt link kan give en anden både adgang og en automatisk enhedsbekræftelse.

I kan sammenligne det samme sikkerhedsnummer under Settings → Encryption, personligt eller gennem et kendt opkald. Kontroller igen, hvis en enhed eller kode skifter. En lokal godkendelse betyder ikke, at alle fremtidige enheder er godkendt.

## Beskytter Tailscale eller iroh det hele?

De beskytter forbindelsen mellem computere, også når Relay-serveren er i åben tilstand. Relays end-to-end-kryptering beskytter indholdet mellem godkendte deltagerenheder, også over for Relay-serveren. At vælge en netværksvej slår ikke i sig selv end-to-end-kryptering fra.

Serveren kan stadig se konti, profiler, medlemskab, størrelser og tidspunkter. Relay giver ikke anonymitet. Tidligere ukrypteret historik bliver ikke privat med tilbagevirkende kraft.

## Kan en adgangskode genskabe mine nøgler?

Nej. Pas på din lokale profil og dine enhedsnøgler. En serverbackup eller en kontoadgangskode alene kan ikke genskabe mistede private nøgler.

## Er programmet godkendt eller scannet?

Udgaven fra 18. oktober 2026 er en usigneret preview. Der er **ikke udført en ny Microsoft Defender-scanning af denne udgave**. Tidligere scanninger gjaldt andre, præcist identificerede filer og er ikke en garanti for nye udgaver.

Der er endnu ikke foretaget et uafhængigt sikkerhedsreview af Relays integration. SHA-256-kontrolkoder viser, om filerne matcher udgivelsen; de beviser ikke, at programmet er uden fejl eller malware.

[Download og kom i gang](README.md) · [Udgivelser](https://github.com/TheBengine/Relay/releases)

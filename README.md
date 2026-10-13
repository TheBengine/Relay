# Relay

**Chat, tale og skærmdeling med lokal hosting og end-to-end-kryptering.**

Relay er en Windows-app til venner og fællesskaber, der ønsker en velkendt, Discord-inspireret brugerflade og mulighed for selv at drive deres server. Fokus er nem kommunikation, kort taleforsinkelse og kontrol over samtalernes indhold.

Dette repository samler projektets beskrivelse og downloadbare Windows-previews. Projektets kildekode offentliggøres ikke her endnu.

## Download og start

**[Hent Relay 0.10.0 til Windows — preview fra 13. oktober 2026](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261013-integrated-network)**

Hent ZIP-filen under **Assets**, pak den helt ud, og åbn kun **Relay.exe** øverst i mappen. Behold hele mappen samlet, inklusive `dist`, `scripts` og `licenses`.

Vil du være vært, vælger du **Start a server on this PC** inde i appen. Relay starter serverne i baggrunden. Du skal kun åbne én startfil; serverstyringen foregår i appen.

Denne udgave tilføjer **indbygget privat netværk med Tailscale-teknologi**, så en separat VPN-app eller netværksdriver ikke skal installeres. Første login og netværksdeling kræver stadig en Tailscale-konto og Tailscales hjemmeside. Reelt login, deling mellem konti og opkald mellem to computere over internettet mangler endnu praktisk afprøvning. Udgaven er fortsat en usigneret preview.

## Hvad kan Relay?

- **Fællesskaber og samtaler:** Servere, tekst- og talekanaler, kategorier, direkte beskeder, gruppesamtaler, tråde, fora og events.
- **Beskeder:** Svar, reaktioner, omtaler, fastgjorte beskeder, afstemninger og filer. Historiksøgning foregår på brugerens computer efter dekryptering.
- **Tale:** Mikrofon- og højttalervalg, push-to-talk, støjreduktion, individuelle lydstyrker og indstillinger til gaming.
- **Video og deling:** Kamera, skærm eller et valgt program, med separate kvalitetsindstillinger og mulighed for at dele programlyd.
- **GIFs:** Online GIPHY-søgning, større visning ved klik og en højrekliksmenu til originalen eller linket. GIF-beskeder viser selve GIF'en uden en synlig URL. Appen har kompakt GIPHY-attribution og indstillinger for automatisk afspilning og reduceret bevægelse.
- **Tilpasning:** Profil- og serverindstillinger, roller og rettigheder, temaer, skalering, notifikationer og genvejstaster. Vinduet kan flyttes og ændres i størrelse.

## Din egen server

En lokal Relay-server kan startes fra appen. Værten bestemmer over fællesskabet og serverens data.

Venner skal kunne nå værtscomputeren. Den nyeste preview har en indbygget privat forbindelse med Tailscale-teknologi, der skal gøre dette lettere på tværs af hjem, også uden en offentlig serveradresse. Værten bruger sin egen konto og lokale server; der kræves ingen fælles Relay-server hos projektets skaber.

Forbind først det private netværk i Relay, log ind med din egen Tailscale-konto, og vælg **Share this community**. Del værtscomputeren via Tailscale og send derefter en Relay-invitation. Begge dele er stadig nødvendige. Læs `LAES-MIG.md` i pakken for den fulde opsætning. Reelt login, kontodeling og brug mellem to computere mangler endnu praktisk afprøvning.

Værtscomputeren og Relay skal være tændt, mens andre bruger serveren. Ved genstart forbindes og deles fællesskabet igen fra appen. Direkte forbindelser foretrækkes; en krypteret netværksrelæserver kan bruges ved vanskelige netværk og kan give højere forsinkelse og mindre streamingkapacitet.

Lokal netværksadgang og manuel offentlig hosting er også mulige. Manuel offentlig hosting kræver en korrekt opsat internetadresse og sikker transport med HTTPS.

## Sikkerhed, kort forklaret

**Hvem kan læse vores samtale?**

Relay krypterer nye beskeder, filer og opkald på deltagernes enheder, før de sendes. Det kaldes end-to-end-kryptering. De godkendte enheder i samtalen har nøglerne til indholdet; serveren videresender det krypterede indhold. Personer, der deltager i samtalen, kan stadig gemme eller videredele det, de modtager.

**Hvorfor beholde Relays kryptering med det private netværk?**

Tailscale krypterer trafikken mellem de tilsluttede computere. Relays end-to-end-kryptering beskytter de krypterede samtalers indhold fra deltager til deltager, også over for selve Relay-serveren. Begge lag er bevaret.

**Hvad kan serveren stadig se?**

Serveren kan se blandt andet konti, offentlige profiler, kanalmedlemskab og forbindelsesoplysninger. Krypteringen beskytter samtalernes indhold; den gør ikke brugerne anonyme.

**Hvordan ved jeg, at jeg taler med den rigtige person?**

Sammenlign jeres sikkerhedskoder eller verificeringskort gennem en kontaktvej, I allerede stoler på, for eksempel personligt eller under et kendt opkald. Godkend derefter de rigtige enheder i Relay. Koden eller kortet kan deles med den anden person og indeholder ingen private nøgler. Hvis koden ændrer sig, skal den kontrolleres igen.

**Hvad betyder “Legacy” ved en besked?**

Det betyder, at beskeden stammer fra den ældre, ukrypterede historik. En opdatering kan ikke gøre allerede gemte, ukrypterede beskeder private med tilbagevirkende kraft.

**Hvad sker der, hvis jeg mister min computer?**

Din lokale historik og dine private enhedsnøgler er vigtige. En kontoadgangskode eller serverbackup alene kan ikke genskabe mistede nøgler. Bevar dem sikkert, og regn ikke med, at en ny installation automatisk giver adgang til alle gamle samtaler.

**Er Relay sikkerhedsgodkendt?**

Relay er en preview under udvikling. Krypteringen er implementeret, men integrationen har endnu ikke fået et uafhængigt sikkerhedsreview. En kompromitteret computer kan stadig give andre adgang til indholdet på den computer.

**Er programfilerne scannet med Microsoft Defender?**

**De nye klient-, server- og netværksfiler i previewen fra 13. oktober 2026 er endnu ikke Microsoft Defender-scannet.** De tidligere resultater nedenfor gælder de præcise filer fra [previewen fra 12. oktober](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261012). De gælder ikke automatisk nye udgaver. Startfilen og video-/streamingserveren er uændrede i den nyeste pakke.

| Programfil | Scanningsdato | Microsoft Defender |
| --- | --- | --- |
| Startfilen (`Relay.exe`) | 12. oktober 2026 | Ingen trusler fundet |
| Relay-appen (`dist/relay.exe`) | 12. oktober 2026 | Ingen trusler fundet |
| Den lokale Relay-server (`dist/relay-server.exe`) | 9. oktober 2026 | Ingen trusler fundet |
| Video-/streamingserveren (`dist/livekit-server.exe`) | 9. oktober 2026 | Ingen trusler fundet |

Dette er et scanningsresultat, **ikke en godkendelse fra Microsoft eller en garanti for fuld sikkerhed**. Andre antivirusmarkeringer er endnu ikke afklaret. Scanningen dækker de nævnte programfiler; launcher-scripts og krypteringens korrekthed er ikke undersøgt med denne scanning. [Microsofts dokumentation om filscanning](https://learn.microsoft.com/en-us/defender-endpoint/command-line-arguments-microsoft-defender-antivirus).

<details>
<summary>Kontrolkoder for de tidligere scannede filer (SHA-256)</summary>

En kontrolkode identificerer den præcise fil, som scanningen gælder for.

| Fil | SHA-256 |
| --- | --- |
| Relay.exe | `7c4caa79aff727cf575413a410a9b5de0acc9c7f869036da514d16c2a859be22` |
| dist/relay.exe | `0fa2f3e35e87eec18a6d3831feaec82889d295fb6c416a9766d0c39f265a31d7` |
| dist/relay-server.exe | `604f3a42e91cf20d5956e1852b45d49508ec6e53f6690426a849964763703f41` |
| dist/livekit-server.exe | `951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1` |

Hver udgivelses ZIP-kontrolkode findes i `.zip.sha256` under **Assets** på dens udgivelsesside. Filoversigten `manifest.json` inde i hver pakke indeholder kontrolkoderne for den pakkes filer.

</details>

**Hvorfor kan Windows advare ved download?**

Den nuværende preview er usigneret. Microsoft SmartScreen kan advare, når en fil ikke downloades særlig ofte, eller udgiveren endnu ikke har opbygget omdømme. Digital signering identificerer udgiveren og kan hjælpe på tværs af udgaver, men garanterer ikke, at advarslen forsvinder. Et antivirusresultat fjerner ikke automatisk denne advarsel. [Microsofts forklaring](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation).

## Tale og streaming

Tale starter med et **modtagebuffer-mål på 10 ms**. Det er en indstilling for, hvor meget lyd Relay gemmer før afspilning; den samlede forsinkelse afhænger også af lydudstyr, computer og netværk.

Streaming tilbyder:

- Opløsning fra **480p til 4K**, samt kildens opløsning inden for 4K.
- **15, 24, 30 eller 60 fps** til skærmdeling.
- Automatisk videobåndbredde eller et valgt loft på **2–32 Mbps**.
- Automatisk, hardwarebaseret eller softwarebaseret videokodning.

Automatisk kvalitetstilpasning kan sænke kvaliteten ved belastning. Højere kvalitetsvalg kræver egnet hardware og forbindelse; længere prøver med fysisk hardware og 4K/60 fps er stadig planlagt.

## Samspil med Discord

Relay udvikles, så venner fortsat kan koordinere i Discord.

En valgfri integration omfatter en **`/relay`-kommando** og én opdateret statusbesked i en valgt Discord-kanal. Den kan vise deltagerantal og værtens tilslutningsinstrukser. Deling er slået fra som standard og kræver opsætning af en Discord-app med bot.

Integrationen er implementeret og lokalt afprøvet. Opsætning og afprøvning mod en rigtig Discord-server mangler stadig. Personlig Discord-profilaktivitet er planlagt.

## Status og næste skridt

Den seneste Windows-preview er **Relay 0.10.0**, udgivet **13. oktober 2026**, med indbygget privat netværk. Download findes på [udgivelsessiden](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261013-integrated-network). Projektet er fortsat under udvikling, og fuld funktionslighed med Discord er et langsigtet mål.

Det næste arbejde omfatter:

- Mere praktisk gennemgang af den indloggede brugerflade.
- Reelt Tailscale-login, netværksdeling mellem konti og opkald mellem to computere på lokale netværk og over internettet.
- Fysisk kamera, programlyd, hardwarekodning og længere 4K/60 fps-prøver.
- Længere målinger af stabilitet og ressourceforbrug.
- Reel Discord-opsætning og integrationstest.
- Færdig installations- og opdateringsoplevelse samt signeret distribution.
- Uafhængig gennemgang af kryptering og sikkerhed.

Relay er et selvstændigt projekt og er ikke tilknyttet Discord. GIPHY bruges til den valgfrie online GIF-søgning.

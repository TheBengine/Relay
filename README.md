# Relay

**Chat, tale og skærmdeling med lokal hosting og end-to-end-kryptering.**

Relay er en Windows-app til venner og fællesskaber, der ønsker en velkendt, Discord-inspireret brugerflade og mulighed for selv at drive deres server. Fokus er nem kommunikation, kort taleforsinkelse og kontrol over samtalernes indhold.

Dette repository samler projektets beskrivelse og downloadbare Windows-previews. Projektets kildekode offentliggøres ikke her endnu.

## Download og start

**[Hent Relay til Windows — installer, 15. oktober 2026](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261015-build/Relay-0.10.0-20261015-build-Setup.exe)**

Åbn **Relay-0.10.0-20261015-build-Setup.exe**, og følg guiden. Installeren lægger programmet i din Windows-brugers programmappe og opretter en genvej i Startmenuen. En skrivebordsgenvej er valgfri. “Open Relay” er valgt som standard, når installationen er færdig. Du skal ikke pakke en ZIP ud eller starte serverfiler selv.

**Opdatering:** Hent den nyere installer, og kør den igen. Den genbruger den eksisterende installation; du skal ikke afinstallere først. Din profil, historik, krypteringsnøgler og serverdata beholdes. Hvis denne pc hoster en server, stop den først i **Server Settings**, og luk derefter Relay. Installeren tvangslukker ingen programmer.

Relay kan afinstalleres gennem Windows’ **Installerede apps**. Det fjerner programfilerne og beholder profilen og serverdata. Automatiske baggrundsopdateringer er endnu ikke aktiveret; opdatering sker ved at køre en nyere installer.

Den valgfrie portable ZIP findes på [udgivelsessiden](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261015-build). Hvis du bruger ZIP-udgaven, skal hele pakkemappen beholdes samlet.

På startskærmen vælger du **Create a server** eller **Join friends**. Guiden **Invite friends** hjælper med deling. **Copy invitation** opretter og kopierer nu invitationen i én handling. Den tidligere Windows-fejl ved **Enable sharing** er rettet. Tailscale-login og godkendelse af privat adgang foregår fortsat i browseren. Brug mellem to pc’er over internettet mangler praktisk afprøvning. Udgaven er en usigneret preview.

## Hvad kan Relay?

- **Fællesskaber og samtaler:** Servere, tekst- og talekanaler, kategorier, direkte beskeder, gruppesamtaler, tråde, fora og events.
- **Beskeder:** Svar, reaktioner, omtaler, fastgjorte beskeder, afstemninger og filer. Historiksøgning foregår på brugerens computer efter dekryptering.
- **Tale:** Mikrofon- og højttalervalg, push-to-talk, støjreduktion, individuelle lydstyrker og indstillinger til gaming.
- **Video og deling:** Kamera, skærm eller et valgt program, med separate kvalitetsindstillinger og mulighed for at dele programlyd.
- **GIFs:** Online GIPHY-søgning, større visning ved klik og en højrekliksmenu til originalen eller linket. GIF-beskeder viser selve GIF'en uden en synlig URL. Appen har kompakt GIPHY-attribution og indstillinger for automatisk afspilning og reduceret bevægelse.
- **Tilpasning:** Profil- og serverindstillinger, roller og rettigheder, temaer, skalering, notifikationer og genvejstaster. Vinduet kan flyttes og ændres i størrelse.

## Din egen server

Serveren ligger på værtens pc. Hver vært beholder sine egne data; projektets skaber skal ikke hoste jeres server.

1. Vælg **Create a server**, og opret din konto. Har du allerede en server, bruger du dens eksisterende konto.
2. Guiden **Invite friends** åbner. Vælg **Enable sharing**, og log ind med din egen Tailscale-konto i browseren første gang. Der skal ikke installeres en separat VPN-app.
3. Åbn guidens delingsside. Vælg **Relay → ⋯ → Share → Copy invite link → Copy share link**, og indsæt linket i guiden. Vælg **Copy invitation**, og send invitationen til din ven, fx i Discord.

Din ven vælger **Join friends** i Relay og indsætter invitationen. Relay klargør den private forbindelse og kontrollerer serveren automatisk. Første gang logger vennen ind med sin egen Tailscale-konto og godkender adgang i browseren. Guiden kan derefter bede om **Try again**. Samme korte forløb bruges ved tilslutning til en ekstra server efter login. Når den forventede server svarer, vises kontoformularen. Vennen skal ikke starte sin egen server for at deltage.

Behold værtscomputeren og Relay tændt, mens andre bruger serveren. Klargøringen kan kort genstarte den identificerede lokale server; konto, kanaler, historik og krypteringsnøgler bevares. Forlad et igangværende opkald først.

Første login, deling mellem konti, serverskift med en reel providerforbindelse og internetopkald mangler endnu praktisk accept. Samlede invitationer kræver denne eller en nyere Relay-udgave; gamle invitationer understøttes stadig. En invitation med et privat adgangslink bør kun sendes til de tiltænkte venner.

Direkte forbindelser foretrækkes. Krypteret videresendelse kan give mere forsinkelse og mindre streamingkapacitet. Manuel offentlig hosting med HTTPS og øvrige netværksmetoder findes under **Advanced**. Den korte danske vejledning ligger i pakkens `LAES-MIG.txt`.

## Sikkerhed, kort forklaret

**Hvem kan læse vores samtale?**

Relay krypterer nye beskeder, filer og opkald på deltagernes enheder, før de sendes. Det kaldes end-to-end-kryptering. De godkendte enheder i samtalen har nøglerne til indholdet; serveren videresender det krypterede indhold. Personer, der deltager i samtalen, kan stadig gemme eller videredele det, de modtager.

**Hvorfor beholde Relays kryptering med det private netværk?**

Tailscale krypterer trafikken mellem de tilsluttede computere. Relays end-to-end-kryptering beskytter de krypterede samtalers indhold fra deltager til deltager, også over for selve Relay-serveren. Begge lag er bevaret.

**Hvad kan serveren stadig se?**

Serveren kan se blandt andet konti, offentlige profiler, kanalmedlemskab og forbindelsesoplysninger. Krypteringen beskytter samtalernes indhold; den gør ikke brugerne anonyme.

**Hvordan ved jeg, at jeg taler med den rigtige person?**

Sammenlign jeres sikkerhedskoder eller verificeringskort gennem en kontaktvej, I allerede stoler på, for eksempel personligt eller under et kendt opkald. Åbn **Settings → Encryption**, vælg **Verify** ved personen, og godkend hver af de rigtige enheder særskilt. Statuskortet viser næste trin. Egen kode og backup kan foldes ud; tekniske detaljer ligger under **Advanced**. En bekræftet enhed på din pc betyder ikke i sig selv, at vennen har bekræftet dig, eller at alle samtaler er klar. Koden eller kortet kan deles med den anden person og indeholder ingen private nøgler. Hvis koden ændrer sig, skal den kontrolleres igen.

**Hvad betyder “Legacy” ved en besked?**

Det betyder, at beskeden stammer fra den ældre, ukrypterede historik. En opdatering kan ikke gøre allerede gemte, ukrypterede beskeder private med tilbagevirkende kraft.

**Hvad sker der, hvis jeg mister min computer?**

Din lokale historik og dine private enhedsnøgler er vigtige. En kontoadgangskode eller serverbackup alene kan ikke genskabe mistede nøgler. Bevar dem sikkert, og regn ikke med, at en ny installation automatisk giver adgang til alle gamle samtaler.

**Er Relay sikkerhedsgodkendt?**

Relay er en preview under udvikling. Krypteringen er implementeret, men integrationen har endnu ikke fået et uafhængigt sikkerhedsreview. En kompromitteret computer kan stadig give andre adgang til indholdet på den computer.

**Er programfilerne scannet med Microsoft Defender?**

**Der er ikke udført en ny Microsoft Defender-scanning af denne preview.** De tidligere resultater nedenfor gælder de præcise filer fra [previewen fra 12. oktober](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261012). De gælder ikke automatisk nye udgaver.

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

Den seneste Windows-preview er **Relay 0.10.0**, udgivet **15. oktober 2026**, med tydelig version og byggedato i appen, mere præcis hjælp til deling og en installer, der tilbyder at åbne Relay bagefter. Den indeholder også den enklere krypteringsside og den korte danske vejledning. Download findes på [udgivelsessiden](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261015-build). Projektet er fortsat under udvikling, og fuld funktionslighed med Discord er et langsigtet mål.

Denne preview består 871 automatiske tests og streng kodekontrol. Velkomst og About er afprøvet ved to vinduesstørrelser, og en isoleret installation og afinstallation har kontrolleret pakkens filer og bevaret testens profildata. Det erstatter ikke praktisk afprøvning mellem to pc’er på forskellige netværk.

Det næste arbejde omfatter:

- Mere praktisk gennemgang af den indloggede brugerflade.
- Enklere forbindelser uden kontokrav samt valgfri ekstra kryptering og målte forskelle i forsinkelse. Disse nye valg er planlagt; den nuværende udgave bevarer E2EE og bruger fortsat Tailscale til den indbyggede private forbindelse.
- Reelt Tailscale-login, netværksdeling mellem konti og opkald mellem to computere på lokale netværk og over internettet.
- Fysisk kamera, programlyd, hardwarekodning og længere 4K/60 fps-prøver.
- Længere målinger af stabilitet og ressourceforbrug.
- Reel Discord-opsætning og integrationstest.
- Automatiske, udgiververificerede opdateringer og signeret distribution; manuel installation og opgradering er nu tilgængelig.
- Uafhængig gennemgang af kryptering og sikkerhed.

Relay er et selvstændigt projekt og er ikke tilknyttet Discord. GIPHY bruges til den valgfrie online GIF-søgning.

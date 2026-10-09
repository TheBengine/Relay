# Relay

**Chat, tale og skærmdeling med lokal hosting og end-to-end-kryptering.**

Relay er en Windows-app til venner og fællesskaber, der ønsker en velkendt, Discord-inspireret brugerflade og mulighed for selv at drive deres server. Fokus er nem kommunikation, kort taleforsinkelse og kontrol over samtalernes indhold.

Dette repository samler projektets beskrivelse, aktuelle muligheder og planlagte forbedringer. Kildekode og installationsfiler offentliggøres ikke her endnu.

## Hvad kan Relay?

- **Fællesskaber og samtaler:** Servere, tekst- og talekanaler, kategorier, direkte beskeder, gruppesamtaler, tråde, fora og events.
- **Beskeder:** Svar, reaktioner, omtaler, fastgjorte beskeder, afstemninger og filer. Historiksøgning foregår på brugerens computer efter dekryptering.
- **Tale:** Mikrofon- og højttalervalg, push-to-talk, støjreduktion, individuelle lydstyrker og indstillinger til gaming.
- **Video og deling:** Kamera, skærm eller et valgt program, med separate kvalitetsindstillinger og mulighed for at dele programlyd.
- **GIFs:** Online GIPHY-søgning, større visning ved klik og en højrekliksmenu til originalen eller linket. GIF-beskeder viser selve GIF'en uden en synlig URL. Appen har kompakt GIPHY-attribution og indstillinger for automatisk afspilning og reduceret bevægelse.
- **Tilpasning:** Profil- og serverindstillinger, roller og rettigheder, temaer, skalering, notifikationer og genvejstaster. Vinduet kan flyttes og ændres i størrelse.

## Din egen server

En lokal Relay-server kan startes fra appen. Værten bestemmer over fællesskabet og serverens data.

Venner skal kunne nå værtscomputeren via det lokale netværk eller en korrekt opsat internetadresse. Ved internetadgang skal værten sørge for sikker transport med HTTPS. En lokal adresse på værtscomputeren gør ikke automatisk serveren tilgængelig for andre.

## Sikkerhed, kort forklaret

**Hvem kan læse vores samtale?**

Relay krypterer nye beskeder, filer og opkald på deltagernes enheder, før de sendes. Det kaldes end-to-end-kryptering. De godkendte enheder i samtalen har nøglerne til indholdet; serveren videresender det krypterede indhold. Personer, der deltager i samtalen, kan stadig gemme eller videredele det, de modtager.

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

**Hvad fortæller en VirusTotal-rapport?**

Den viser antivirusresultater for en bestemt programfil. Den kontrollerer ikke, om Relays kryptering er implementeret korrekt, og er ingen garanti for fuld sikkerhed. [Læs om VirusTotal](https://docs.virustotal.com/docs/how-it-works).

De tre programfiler i **Relay 0.10.0 navigation-preview** blev analyseret **9. oktober 2026**:

| Program | Detektioner | VirusTotal |
| --- | --- | --- |
| Relay-appen | **1/57** | [Se rapport](https://www.virustotal.com/gui/file/2d0ac2b71c6a41a2a98bd510fbbe70d7bb7ee22eb3219288f9a3ec3b17c8cfd0/detection) |
| Den lokale Relay-server | **2/67** | [Se rapport](https://www.virustotal.com/gui/file/604f3a42e91cf20d5956e1852b45d49508ec6e53f6690426a849964763703f41/detection) |
| Video-/streamingserveren (LiveKit) | **0/70** | [Se rapport](https://www.virustotal.com/gui/file/951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1/detection) |

**Fundene er endnu ikke afklaret.** Trapmine markerede Relay-appen. ClamAV og Trapmine markerede den lokale server. Vi har ikke fastslået, om disse fund er fejlmeldinger, og udgaven beskrives derfor ikke som en sikkerhedsgodkendt udgivelse.

Microsofts antivirusmotor rapporterede ingen fund i de tre filer. Nogle øvrige motorer overskred svartiden, fejlede eller understøttede ikke filtypen; tallene viser motorer, der returnerede en vurdering. Rapporter kan ændre sig ved senere analyser. Scanningen dækker disse tre programfiler, ikke en gennemgang af kryptering eller alle launcher-scripts.

<details>
<summary>Filernes kontrolkoder (SHA-256)</summary>

En kontrolkode identificerer den præcise fil, som rapporten gælder for.

| Fil | SHA-256 |
| --- | --- |
| relay.exe | `2d0ac2b71c6a41a2a98bd510fbbe70d7bb7ee22eb3219288f9a3ec3b17c8cfd0` |
| relay-server.exe | `604f3a42e91cf20d5956e1852b45d49508ec6e53f6690426a849964763703f41` |
| livekit-server.exe | `951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1` |

</details>

**Hvorfor kan Windows advare ved download?**

Den nuværende preview er usigneret. Microsoft SmartScreen kan advare, når en fil ikke downloades særlig ofte, eller udgiveren endnu ikke har opbygget omdømme. Digital signering identificerer udgiveren og kan hjælpe på tværs af udgaver, men garanterer ikke, at advarslen forsvinder. En VirusTotal-rapport fjerner ikke automatisk denne advarsel. [Microsofts forklaring](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation).

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

Den seneste lokale Windows-preview er **Relay 0.10.0**, leveret **9. oktober 2026**. Projektet er fortsat under udvikling, og fuld funktionslighed med Discord er et langsigtet mål.

Det næste arbejde omfatter:

- Mere praktisk gennemgang af den indloggede brugerflade.
- Opkald mellem to computere på lokale netværk og over internettet.
- Fysisk kamera, programlyd, hardwarekodning og længere 4K/60 fps-prøver.
- Længere målinger af stabilitet og ressourceforbrug.
- Reel Discord-opsætning og integrationstest.
- Færdig installations- og opdateringsoplevelse samt signeret distribution.
- Uafhængig gennemgang af kryptering og sikkerhed.

Relay er et selvstændigt projekt og er ikke tilknyttet Discord. GIPHY bruges til den valgfrie online GIF-søgning.

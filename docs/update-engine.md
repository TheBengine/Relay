# Verificerede opdateringer – motor, status, genstart og installerhandoff

## Aktuel implementering — bootstrap .7 klargøres

Kilden indeholder nu en rigtig TUF-udgiverrod og beta-adresserne
`https://thebengine.github.io/Relay/updates/beta/metadata/` og
`https://thebengine.github.io/Relay/updates/beta/targets/`.
Rodens SHA-256 er `383afc351bd45cf480be147e2a23f5831be25ec04f179e08acee3bbc188c7770`.
Den kommende `.7`-pakke og metadata er endnu ikke erklæret offentliggjort.

For metadata med **handoff: in-place-v2** betyder **Update and restart** ét valg:
hent og kontrollér den præcise installer, luk appen og dens egne lokale servere
normalt, opdatér den verificerede eksisterende installation uden installationsguide,
og genåbn Relay. **Download only** henter uden installationssamtykke. Opkald og
ventende arbejde spærrer fortsat; en lokal vært kan lukkes som del af v2-forløbet.
Portabel kørsel beholder manuel installation. Den ældre `wait-parent-v1`-sti
bevarer installationsguiden og kræver, at lokal hosting først stoppes.

Udgave `.6` og ældre skal have den nye bootstrap installeret manuelt én gang.
TUF beskytter efterfølgende opdateringers identitet og integritet; det er ikke
Authenticode, og Windows kan stadig advare ved den første manuelle installation.
Produktionsnøglerne er DPAPI-beskyttet under `C:/Users/benja/.relay-publisher`.
To rodnøgler i samme Windows-profil er ikke uafhængig nøgleopbevaring eller en
portabel backup. Se [udgiveropsætningen](publisher-setup.md).

Timestamp og snapshot udløber efter 30 dage. Operatøren skal inden da genklargøre
samme pakke med uændrede noter/handoff og offentliggøre nye metadataversioner;
ingen automatisk fornyelse er planlagt. De aktuelle kandidatprøver og offentlig
distribution skal kvitteres særskilt. Tidligere testtal og status nedenfor er historik.

## Historiske forløb før produktionsbootstrap

Windows x64-klienten kontrollerer nu nyere udgivelser 30 sekunder efter opstart og derefter hver sjette time. Beskeden øverst tilbyder **Download update** og **Later**; samme status findes under Advanced → Updates, hvor automatiske kontroller kan slås fra. Baggrundskontrollen udskydes under opkald. Downloads og installation kræver stadig brugerens valg.

Kontrol 25. oktober 2026: native-RTC-klienten kompilerer; 12 målrettede UI/versionskontroller og én eksisterende handoff-afvisningsprøve består. Den ændrede Inno-installer er kompileret med den eksisterende previews payload alene som kompileringskontrol. Den er ikke installeret eller udgivet. Genåbning efter en faktisk installation og den samlede signerede produktionskanal er ikke afprøvet i denne ændring. Tidligere kontroltal nedenfor er historiske.

Produktion har fortsat ingen rigtig bundlet udgiverrod/adresse. Den særskilte GitHub-kontrol henter højst ti offentlige releases med en begrænset forespørgsel uden konto-oplysninger. Den sammenligner den præcise Build-sekvens og kræver det officielle installer-asset. Resultatet er kun en besked med et fast GitHub-link; det skaber aldrig en PreparedInstaller eller installationstilladelse. Ugyldig udgiverpolicy falder ikke tilbage til denne kanal. Ingen opdateringsfiler eller personlige mapper oprettes af GitHub-kontrollen. Installeren er fortsat usigneret, og download i denne preview åbner udgivelsessiden til manuel installation.

Handoff findes for signerede releases, der erklærer **handoff: wait-parent-v1**. Feltet er valgfrit i ældre metadata; de releases beholder Show download og får ingen eksekveringstilladelse. Andre protokoller afvises. En klar download vises med brugerens **Install and reopen Relay**-valg, når opdatering senere bliver korrekt konfigureret. Download eller startup betyder ikke installationssamtykke. Installationsguiden skal stadig gennemføres; dette er endnu ikke en fuld ubemandet opdatering.

Den sidste hash-/størrelses-/udløbskontrol kører i en enkelt kortlivet arbejder. Det faktisk åbnede installerfile og alle navngivne forældremapper fastholdes mod skrivning/sletning/flytning under kontrol, udpakning, klar-signal og endeligt samtykke. Reparse points og hardlinks afvises. Ingen shell eller tvangsstop bruges i produktionskaldet. Det faste RELAYRESTART-flag accepteres kun sammen med en gyldig handoff og aktiveres først efter samtykke og præcis parent-exit. Efter vellykket installation og afsluttede installationsrecords åbnes den installerede Relay igen; den venter på installerens afslutning. Manuelle installeringer beholder deres valg på Finish-siden. Kontrollen kræver en resterende sikkerhedsmargin før metadataudløb.

Tre tilfældige lokale Windows-events giver klar-, samtykke- og annulleringssignaler. Installeren åbner en proceshandle og sammenligner proces-ID samt oprettelsestid. Den venter højst30 s på samtykke og derpå højst60 s på faktisk afslutning af netop den proces, inden normal installationsguide starter. Cancel har forrang. Manuel installerstart uden handoffparametre bevarer det normale flow. Malformed/manglende events, forkert procesidentitet og timeouts starter ingen installation.

Appen behandler ventende netværksopdateringer før den sidste opkaldskontrol. Aktiv/opkoblende tale, lokal vært, igangværende markerede operationer og ventende beskeder spærrer eller annullerer handoff. Efter det endelige samtykke sendes normal lukning, og den frame udfører ingen flere UI-handlinger. Et sent resultat fra en allerede afsluttet installer afvises, og Relay bliver åbent. Den eksisterende normale preference-/media-/network-lukning bruges; der foretages ingen actor-/ratchetkopi eller nøgle-/profilnulstilling. Native konfigureret app-lukning med egne konti er stadig en særskilt restaccept.

Arbejderen har ingen idletråd eller polling. Resultatet vækker UI én gang; kun den korte result/JoinHandle-afslutningsrace bruger5 ms repaint. Annullering/Drop frigiver ventende installer via event, uden processkill. Tidligere TUF-policy, HTTPS/WSS, vedvarende immutable metadata og downloadgenstart/cachegrænser fra [storagefase3](verification-update-storage-0.10.0.json) bevares. Cache er fortsat1 GiB plus128 KiB,512 poster,512 MiB pr. installer; kun kendte verificerede filer slettes, ukendte/ændrede filer bevares eller afvises.

730 almindelige tests,54 updaterprøver, streng kontrol, formatering og ny native klientrelease fra674 inputs består. To yderligere eksplicitte signerede fixtureprøver bruger den faktisk kompilerede isolerede Inno-installer gennem den samme TUF/download/hash/pin/launch/ready-sti: normal annullering og faktisk30 s timeout med afvisning af sent resultat. Private signernøgler findes alene i testhukommelse. Testkaldets silent-/egen-mappeflags er ikke et produktionsvalg. Final pending-send-gates ændrede kun to callerfiler efter disse prøver; den faktisk udøvede updater-/dependency-/installerkode er hashidentisk, og endelige samlede checks omfatter callerne.

Fem faktiske isolerede installerprøver accepterer forkert procesoprettelsestid, cancel før/efter samtykke, manglende samtykke og normal installation først efter egen forældreprocesses afslutning. Positiv prøve inkluderer reinstall/uninstall og bevaring af ukendt fil. Første harness brugte forkert registreringsnøgle; den successfulde ejede installation blev genverificeret og normalt afinstalleret, og fejlen bevares. Præcis ny produktionsinstaller install/reinstall/uninstall bevarer egne profiler/syntetiske sentinels/unknown fil og fjerner egen registrering. Ingen personlig installation, fysisk/visuel app-UI, strømsvigt eller afbrudt rigtig installation tilskrives disse prøver.

[Aktuel kontrol](verification-update-handoff-0.10.0.json). [67](tasks/67-verified-native-updates.md) beholdes for konfigureret native app-handoff/normal kontolukning, faktisk installerafbrydelse/recovery og samlet afslutning. Reel udgiverrod/endpoints/certifikat/offentlig betroet distribution, uafhængigt review, fysisk hardware/WAN og hele målet forbliver åbne under67/07 og de øvrige opgaver. [Historisk appfase2](verification-update-ui-0.10.0.json), [motorfase1](verification-update-engine-0.10.0.json).

Windows-protokollen følger [procesoprettelsestid](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes) og [deling af åbne filer](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilea). Den konkrete adfærd og dens grænser er kontrolleret i de angivne fixtures.

## Recoveryfase5

[Installer- og opstartskontrol](installation-recovery.md) og [prøvebevis](verification-installer-recovery-0.10.0.json) er integreret. Tidligere afsnits manglende installerrecovery er historisk; konfigureret native konto-handoff, rigtig udgiveropsætning og hele67 er fortsat åbne.

## Softwarefase69

[Normal lukning og versioner](native-shutdown.md) og [kontrol](verification-native-shutdown-0.10.0.json) er integreret. Opdatering venter på kontoarbejde og faktisk kontrolloop-afslutning med HTTP-kvittering. Rigtig udgiver og indlogget native konto-/opdateringsaccept forbliver åbne i67.

## Native kontofase70

[Login og normal genstart](native-account-lifecycle.md) og [kontrol](verification-native-account-lifecycle-0.10.0.json) supplerer fase69. Den aktuelle normale app er faktisk kontrolleret med og uden husket login, normal Close, genstart uden adgangskode og eksplicit Log Out i egne nye konti/profiler. Tidligere afsnits åbne normale konto-/genstartsaccept er dermed historisk. Faktisk opdateringssamtykke og installerhandoff gennem en konfigureret app kræver stadig særskilt accept under67; almindelig Close er ikke bevis for den sti. Produktionsudgiveropsætning og hele målet er fortsat åbne. Leveret pakke og produktkode er uændrede.

# Installation og reparation – fase5

## Genstart efter afsluttet installation — 5. november 2026

Installeren kan nå at afslutte, før den genstartede klient åbner dens proces. Hvis Windows angiver, at proces-ID'et ikke længere findes, fortsætter klienten til den normale kontrol af installationens statusmarkører. En uafsluttet installation spærrer fortsat opstart. En observerbar proces skal stadig matche oprettelsestiden og afslutte inden for ventetiden.

En isoleret Windows-test bekræftede fejlen før rettelsen ved at afslutte en rigtig proces og frigive dens sidste testhandle. Efter rettelsen bestod alle 74 protokoltests. Afprøvning med den færdige signerede pakke på ren Windows er stadig en separat udgivelseskontrol.

## Tidligere installationskontrol

9. oktober2026. [Kontrol](verification-installer-recovery-0.10.0.json). Ny23-fils Downloads-pakke: `C:/Users/benja/Downloads/Relay-0.10.0-20261009-installation-repair`.

Klient og server kontrollerer offentlig installationsstatus, før profiler, nøgler, historik eller serverdata åbnes. Pending-/temp-markering, beskadiget markering eller ufuldstændig installation spærrer normal opstart. Klienten har et lille reparationsvindue; serveren afslutter med en offentlig forklaring. Den rigtige installer kan gendanne programfiler i samme programmappe med samme eller nyere pakke. Ukendte filer bevares. Beskadigede/modstridende markeringer og links afvises uden automatisk sletning; en ny tom programmappe kan vælges uden at nulstille profilen.

Installer og afinstaller bruger en fælles lås. Journal og monoton markør offentliggøres før payload; færdig markering kontrolleres før pending fjernes. Opstart efter installation venter på installerens præcise procesidentitet. Windows kan vælge unins001 efter en afbrydelse; kontrol og cleanup bruger den faktisk registrerede afinstaller frem for at antage unins000. Markeringer er ikke udgiversignaturer. En portabel app uden markering kan starte normalt.

743 workspace-prøver, herunder 17 GIF-, 54 updater- og 9 startup-prøver, streng kontrol, formatering og native klient/server-release består fra 677 identiske frosne inputs. Testsuiten har desuden 58 ignorerede prøver; de regnes ikke som bestået. 38 faktiske startup-diagnoser fra de nye binaries består uden profil-/serverdataopstart. 10 første-/opdateringsafbrydelser bruger de bevarede v1-installerkontroller. To yderligere afbrydelser bruger den endelige kontrol med korrekt afinstallationsfil: blandet opdateringspayload og afsluttet førstegangs-payload. 31 negative workflowtrin omfatter 14 reelle installer-/afinstalleroperationer for malformed/truncated/forkert app-id/konflikt/hardlink/reparse/temp-only/lås. Den nøjagtige endelige produktionsinstaller består install/reinstall/uninstall med separate syntetiske sentinels og ukendt brugerfil.

Historiske fejl er bevaret: dead-PID-fixture antog forkert at et afsluttet procesobjekt straks var utilgængeligt; afinstallerfixturen antog unins000; én efterfølgende case blev afvist før opstart på grund af den tilbageværende egen registrering. De relevante nye prøver og normale egne oprydningsreceipts er dokumenteret separat. Historiske harness-assertions bliver ikke omskrevet til fulde historiske indholdshashes; rapporten skelner gemt evidens fra friske slutobservationer.

Ingen fysisk reparationsvindue-/konto-handoff-/muse-/clipboardaccept eller vilkårligt strømsvigt påstås. Samtidige ældre installere deltager ikke i den nye lås. Rigtig udgiverrod, certifikat, offentlig betroet distribution og eksternt sikkerhedsreview er åbne; installeren er usigneret, onlineopdatering er fra.67 beholdes for punkt4 og samlet afslutning;53/51 fysisk GIF-accept og hele Relay-målet er åbne. Lokal hosting, E2EE,10ms-standard, Tonepar og GIF-klik/menu bevares.

Pakkevejledningen OPDATERINGER.md bevarer historiske faseafsnit med 730/674 og genbrug af serveren. De beskriver ældre pakker; denne pakke har 743 beståede prøver fra 677 inputs, ny klient/server og byteidentisk genbrugt medieserver. LAES-MIG.md beskriver den aktuelle pakke. Statisk kontrol af native afinstallationsfiler er ikke bevis for hvert deletion-target i Windows-installationsloggen.

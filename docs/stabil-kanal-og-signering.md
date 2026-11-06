# Stabil kanal og Authenticode (#30)

Kun **beta** er i drift (`crates/relay-updater/publisher/policy.json`). Pakkebygningen har nu en valgfri Authenticode-vej. Den første signerede kandidat kræver et betroet kodesigneringscertifikat med adgang til den tilhørende nøgle.

## Hvad er forberedt

| Del | Hvad | Hvor |
|---|---|---|
| Stable-politik | Samme gennemgåede root (`383afc35…`) som beta, egne HTTPS-adresser under `/updates/stable/` | `crates/relay-updater/publisher/policy-stable.json` |
| Byg-valg | `scripts/build.ps1 -Channel stable` kompilerer stable-politikken. Standard er beta. Den byggede klient viser sin faktiske kanal i `--build-info` | `scripts/build.ps1`, `crates/relay-updater/src/publisher_policy.rs` |
| Klientregel | En stable-klient afviser prerelease-versioner (fandtes allerede) | `src/lib.rs` (`publisher.channel == "stable"`) |
| Udgiverport | Stable kontrollerer udgiver, tidsstempel og filaftryk og genberegner de syv tests fra PC A og PC B's oprindelige måle-ZIP'er, før nøglebutikken åbnes | `scripts/publish-updates.py` |

## Det kræver stabil kanal, før første udgivelse

1. **Authenticode-certifikat** (se nedenfor), og signering af `Relay.exe`, `dist\*.exe`, afinstalleringsprogrammet og installeren med tidsstempel **før** installer-hashen lægges i TUF-metadata.
2. **Bestået to-PC-accept** for alle syv tests med samme build-sekvens (`docs/accepttest-to-pc.md`).
3. **Afprøv den signerede stable-kandidat lokalt** som en forhåndsudgave. Eksisterende beta-klienter fortsætter på beta.
4. **Uafhængig root-forvaring.** I dag ligger begge root-nøgler i samme DPAPI-butik (`C:\Users\benja\.relay-publisher`). Tærsklen 2-af-2 er dermed opfyldt kryptografisk, men ikke ved adskilt forvaring. Mindst én root-nøgle bør flyttes offline (fx krypteret USB i skuffe). Det er en planlagt root-ændring og laves ikke her.
5. **Offentlig stable-mappe** `https://thebengine.github.io/Relay/updates/stable/` oprettes først ved første rigtige stable-udgivelse.
6. **Plan for en dårlig stable:** En klient, der allerede har installeret versionen, skal have en ny kandidat med højere sekvens. Afprøv den signerede rettelse, før den udgives på stable.

## Byg og kontroller en signeret pakke

Byg med `scripts/build.ps1 -Channel stable` og de forberedte SDK- og cacheparametre i [byggevejledningen](building.md). Klargør den binære distribution med `manifest.json` og `build-info.json` fra den præcise klient. Kontrollér `client_build.update_channel` er `stable`. Vælg et kodecertifikat i `Cert:\CurrentUser\My`, den lokale Windows SDK-version af SignTool og certifikatudbyderens godkendte HTTPS-adresse til RFC 3161-tidsstempler. Kør builderen med absolutte lokale stier.

```powershell
python scripts/build-windows-installer.py --payload $payload --manifest $manifest `
  --compiler $iscc --output $newOutput --sequence $sequence --require-build-identity `
  --sign-tool $signtool --signer-thumbprint $thumbprint --timestamp-url $timestampUrl
```

Builderen signerer kopier af launcher, klient, server og de medfølgende binære hjælpere i outputmappen og opdaterer filaftrykkene. Inno klargør afinstalleringsprogrammet, som builderen signerer og kontrollerer, før Inno indlejrer det i installeren. Installeren signeres til sidst. Windows skal godkende hver indlejret signatur, dens tidsstempel og det valgte certifikat, før builderen gemmer en fuldført `build.json`.

Hvis du også laver en portable ZIP, skal den indeholde filerne fra den signerede `stage/manifest.json` samt selve manifestet. Udgivelsesposten `package.json` skal bruge `build.json.client_sha256` og `build.json.authenticode.signer_thumbprint` som `authenticode_signer_thumbprint`. Installerens SHA-256 og størrelse måles efter signering.

```powershell
python scripts/publish-updates.py stage-candidate --package $package --installer $installer `
  --build-receipt $buildJson --notes $notes --output $candidate
```

Sæt `$buildJson` til `build.json` i installer-builderens outputmappe. Klargøringen sammenholder den signerede payload-manifestfil og byggekvitteringen. Windows kontrollerer signaturen og filaftrykket på installeren, hver medfølgende EXE og Innos afinstallationsprogram. Kandidatmappen gemmer et afgrænset udsnit af byggekvitteringen som `signed-build.json`, og udgivelsesscriptet sammenholder dens filaftryk igen, før det åbner nøglebutikken.

Brug `Relay-Setup.exe` fra kandidatens mappe til Windows- og to-PC-accepten. Kør `verify-installed` efter først en ny installation på ren Windows og dernæst en opgradering fra beta. Brug den faktiske programmappe for hver installation. Kontrollen læser alle pakkens installerede filer, Windows' afinstallationsregistrering og installationsmarkørerne og gemmer en kvittering til hver test.

```powershell
python scripts/publish-updates.py verify-installed --candidate $candidate `
  --installed $freshProgramDir --scenario fresh-install --output $freshCheck
python scripts/publish-updates.py verify-installed --candidate $candidate `
  --installed $upgradedProgramDir --scenario beta-upgrade --output $upgradeCheck
```

Scenariet angives af testeren. Notér separat, hvilken Windows-installation og beta der blev brugt, og hvad der skete under afbrydelse og gendannelse. Kør måleren med `-StableCandidate` og behold de 14 oprindelige ZIP-filer. Klargør derefter de signerede opdateringsmetadata med samme kandidatmappe, de to Windows-kvitteringer og målefilerne.

```powershell
python scripts/publish-updates.py publish --channel stable --store $store `
  --candidate $candidate --acceptance $bundles `
  --windows-install-checks $freshCheck $upgradeCheck --output $newPublication
```

Kandidatmappen er lokal og opretter ingen TUF-metadata. `publish --channel beta` tager kun en beta-klient eller en ældre beta-pakke uden kanalfelt. En installer bygget til stable ville ellers skifte en beta-installation til den stabile kanal. Offentlig upload er et særskilt trin efter godkendelse.

Ved manuel opgradering fra beta bruger stable sit eget opdateringslager. Kontoen, profilen og det eksisterende beta-lager bevares, så stable kan kontrollere opdateringer uden at overtage betas metadata.

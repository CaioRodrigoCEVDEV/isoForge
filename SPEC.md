# ISOForge — Especificação técnica 0.2

## Objetivo

Aplicação desktop nativa para Linux, em Rust + GTK4, para inspecionar, editar e
reconstruir imagens ISO preservando ISO9660, UDF e as estruturas de boot
El Torito (BIOS e UEFI) quando o formato permitir uma reconstrução segura.

## Princípios

1. A ISO original nunca é modificada nem sobrescrita.
2. Toda edição ocorre em um workspace temporário em disco.
3. A exportação escreve em um arquivo temporário e só renomeia após uma
   verificação estrutural mínima.
4. Nenhum comando externo é interpolado em shell; tudo usa `std::process::Command`.
5. UDF é obrigatório quando presente: não se descarta UDF silenciosamente.
6. A camada UDF é a fonte de verdade em imagens híbridas.
7. Não se anuncia uma ISO como inicializável sem teste real de boot em VM.
8. Metadados de boot desconhecidos ou mídia híbrida MBR/GPT bloqueiam a
   reconstrução em vez de serem descartados.

## Arquitetura

```text
GTK4 UI (src/main.rs)
  ├── Abrir ISO / árvore de arquivos / adicionar / remover
  ├── Tela de inspeção antes da exportação
  ├── Barra de progresso e cancelamento
  └── Relatório de validação pós-exportação
              |
              v
core (src/core/)
  ├── iso_inspector.rs   IsoMetadata, camadas, El Torito, área de sistema
  ├── iso_builder.rs     IsoBackend, HybridIsoBackend, XorrisoBackend, UdfBackend
  ├── iso_validator.rs   Checks PASS/WARNING/FAIL, QEMU/OVMF (detecção)
  ├── model.rs           IsoMetadata, BootEntry, BackendCapability, relatórios
  ├── process.rs         execução capturada, progresso, cancelamento, espaço livre
  └── fsutil.rs          manifesto do workspace, permissões, tamanhos
```

### Interface de backend

```rust
pub trait IsoBackend {
    fn name(&self) -> &'static str;
    fn capability(&self, metadata: &IsoMetadata, tree: Option<&Path>) -> BackendCapability;
    fn rebuild(&self, request: &BuildRequest, cancel: &CancelToken,
               progress: &mut dyn FnMut(Progress)) -> CoreResult<BuildResult>;
}
```

Seleção:

- `has_udf && has_iso9660` → `HybridIsoBackend` (genisoimage, UDF + El Torito).
- `!has_udf && has_iso9660` → `XorrisoBackend` (`xorriso -as mkisofs`).
- `has_udf && !has_iso9660` → `UdfBackend` somente leitura (reconstrução
  explicitamente não suportada).
- Área de sistema MBR/GPT/APM → bloqueio explícito em ambos os backends.

## Inspeção

`inspect()` executa xorriso, blkid, 7z e leitura binária direta:

- PVD ISO9660 em LBA 16: volume ID, system ID, versão, extensão raiz.
- Joliet: descritores suplementares e sequências de escape.
- Rock Ridge: assinatura `SP` no diretório raiz.
- UDF: Volume Recognition Sequence (`BEA01`, `NSR02`/`NSR03`) e AVDP no LBA 256,
  complementado por `blkid` (versão 1.02 neste caso).
- El Torito: `xorriso -report_el_torito plain`, com fallback de leitura direta
  do boot record e do catálogo.
- Área de sistema: assinatura MBR `0x55AA`, entradas de partição e `EFI PART`.
- Arquivos essenciais: `/bootmgr`, `/efi/boot/bootx64.efi`,
  `/efi/microsoft/boot/bootmgfw.efi`, `/efi/microsoft/boot/efisys.bin`,
  `/boot/etfsboot.com`, `/sources/install.wim`, `/sources/install.esd`,
  `/sources/boot.wim`, `/boot.catalog`, `/autounattend.xml`.

### Descoberta crítica: camadas ISO9660 e UDF divergentes

Na ISO real de teste:

- ISO9660 `/sources/install.wim`: 2.411.208.172 bytes.
- UDF `/sources/install.wim`: 6.706.175.468 bytes.

O ISO9660 expõe uma visão de tamanho limitado; o conteúdo completo só existe na
camada UDF. Por isso:

1. A inspeção lista **as duas camadas** e registra a divergência como aviso.
2. A extração usa `7z x -tUdf` sempre que UDF está presente. Se o 7z faltar, a
   extração é recusada — nunca se cai silenciosamente para ISO9660.
3. A validação compara o workspace e a ISO gerada pela camada UDF.
4. A diferença de tamanho entre as camadas geradas é reportada como WARNING,
   explicada por `-allow-limited-size`, sem falhar a validação.

## Reconstrução

### 1. Extração

- UDF presente: `7z x -y -tUdf -o<workspace> <iso>`.
- Sem UDF: `xorriso -osirrox on:auto_chmod_on -indev <iso> -extract / <workspace>`.
- O workspace fica em `$XDG_CACHE_HOME/isoforge` (ou `ISOFORGE_WORK_DIR`), nunca
  em `/tmp`, com verificação de espaço livre e ajuste de permissões.

### 2. Imagens de boot

Para cada entrada El Torito bootável (BIOS/UEFI):

- A imagem é lida da ISO original em `LBA * 2048`, com
  `load_size * 512` bytes.
- Se o arquivo correspondente no workspace diferir, ele é substituído pela
  imagem real extraída do LBA. Isso corrige o caso de `boot/etfsboot.com`
  pré-patcheado.
- A imagem UEFI (`efisys.bin`) também é comparada/restaurada.
- Nunca se usa `-boot-info-table`.

### 3. Geração

`HybridIsoBackend` (genisoimage, UDF):

```bash
genisoimage -udf -allow-limited-size -iso-level 4 -D -N \
  -c boot.catalog \
  -b boot/etfsboot.com -no-emul-boot -boot-load-size 8 \
  -eltorito-alt-boot \
  -e efi/microsoft/boot/efisys.bin -no-emul-boot -boot-load-size 2880 \
  -V "<VOLUME_ID>" -o <temp.iso> <workspace>
```

`XorrisoBackend` (sem UDF) usa os mesmos argumentos via
`xorriso -as mkisofs`, sem `-udf`.

Depois da geração:

1. Inspeção rápida do arquivo temporário.
2. Comparação de UDF, ISO9660, El Torito, caminhos/tamanhos de carga e bytes das
   imagens de boot.
3. Comparação do tamanho UDF de `install.wim`/`install.esd`.
4. Em caso de falha, o temporário é apagado e a exportação falha.
5. Em sucesso, o temporário é renomeado atomicamente para o destino.

### Avaliação de `-boot_image any replay`

A abordagem `xorriso -indev ORIG -outdev NEW -boot_image any replay -map TREE /`
foi testada. Ela preserva os bytes das imagens BIOS/UEFI, mas:

- não escreve UDF (limitação do libisofs);
- emitiu aviso de imagem El Torito oculta e perdeu a associação do caminho da
  imagem BIOS no teste realizado.

Por isso não é usada para imagens híbridas. A reconstrução explícita com
genisoimage e imagens de boot restauradas é o caminho padrão. Para ISO9660 puro,
o backend usa `xorriso -as mkisofs` com os mesmos parâmetros explícitos.

## Validação

`validate()` gera checks com estados PASS, WARNING ou FAIL:

- arquivo de saída existe e tamanho;
- presença de ISO9660 e UDF, e versão UDF;
- El Torito e catálogo;
- entradas BIOS/UEFI, caminho, load size e bytes da imagem de boot;
- BOOTMGR, BOOTX64.EFI/BOOTMGFW.EFI, INSTALL.WIM/INSTALL.ESD;
- etfsboot.com e efisys.bin;
- integridade de todos os arquivos do workspace com os tamanhos da camada UDF;
- tamanho UDF de `install.wim`/`install.esd` comparado à original;
- consistência ISO9660/UDF (WARNING quando o ISO9660 usa tamanho limitado);
- `autounattend.xml` quando presente no workspace;
- área de sistema, se a original tivesse MBR/GPT;
- teste de boot em VM.

Estados finais:

- **ISO inicializável validada** — apenas se um teste real de boot passar.
- **ISO criada e validada estruturalmente; boot não testado em VM** — checks
  obrigatórios passaram, mas QEMU/OVMF não foram executados.
- **Reconstrução inválida — não use para boot** — qualquer FAIL.

QEMU/OVMF são detectados; a execução automática de boot ainda não está
implementada. A presença dos arquivos de boot nunca é tratada como prova de
inicialização.

## Formatos e limitações

| Formato | Reconstrução | Observação |
|---|---|---|
| ISO9660 / ISO9660:1999 + El Torito | suportada | xorriso |
| ISO9660 + UDF 1.02 + El Torito | suportada | genisoimage, UDF autoritativo |
| ISO9660 sem boot | suportada como ISO de dados | não inicializável |
| UDF puro | somente leitura | não há backend UDF seguro |
| MBR/GPT/APM híbrido | bloqueada | não descartar metadados |

Limitações adicionais:

- Não há teste automático de boot em VM (QEMU/OVMF ausentes neste host).
- A camada ISO9660 de arquivos grandes pode ter tamanho limitado; a UDF contém
  o conteúdo completo. O relatório explica a diferença.
- Nomes não-ASCII dependem do suporte de charset do genisoimage.
- SIGKILL pode deixar workspaces órfãos no cache.

## Segurança e integridade

- Original somente leitura; destino não pode ser a ISO original.
- Escrita em temporário no diretório de destino e rename atômico.
- Verificação de espaço livre antes da extração e da reconstrução.
- Captura de stdout/stderr/código de saída; erros não são ignorados.
- Cancelamento mata o processo externo e remove o temporário.
- Extração UDF obrigatória quando UDF existe; nunca fallback silencioso.
- Workspaces com permissões somente leitura são tornados graváveis para permitir
  remoção e limpeza.

## Testes

`cargo test` cobre:

- inspeção de ISO via variável `ISOFORGE_TEST_ISO`;
- backend híbrido com UDF e boot BIOS/UEFI;
- backend xorriso sem UDF;
- validação detectando perda de UDF e BOOTMGR ausente;
- árvore de arquivos.

Teste real (ignorado por padrão):

```bash
ISOFORGE_REAL_ISO=/caminho/Windows11.iso \
  cargo test -- --ignored real_iso_roundtrip --nocapture
```

Resultado executado com `Windows11_Ultra_Performance_SINGLE_25H2_BR_x64.iso`:

- workspace UDF ~7,1 GB;
- ISO gerada ~7,1 GB, UDF 1.02;
- `install.wim` UDF 6.706.175.468 bytes preservado;
- BIOS e UEFI byte a byte idênticos à original;
- BOOTMGR, BOOTX64.EFI, INSTALL.WIM, etfsboot.com e efisys.bin presentes;
- `autounattend.xml` presente;
- 1066 arquivos do workspace presentes com tamanhos esperados;
- validação estrutural PASS; teste de boot em VM não executado (WARNING).

## Próximas entregas

- Execução automática de boot com QEMU/OVMF/SeaBIOS e critério de sucesso real.
- Backend para replay de MBR/GPT/APM híbrido.
- Backend UDF puro com ferramentas UDF dedicadas.
- Persistência de projeto e importação de diretórios.

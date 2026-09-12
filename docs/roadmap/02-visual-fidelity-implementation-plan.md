# Phase 2 — Plano ordenado para fidelidade visual completa

## Objetivo

Fazer os cenários rural, denso, água e stress renderizarem os assets estáticos da Phase 2 sem
fallback branco, planos opacos indevidos, texturas obrigatórias ausentes, terreno incoerente ou
geometria visível desaparecendo. O trabalho termina somente quando a campanha completa produzir
`accepted`, com baseline e revisão visual assinada.

O escopo continua limitado a `STAT`, `MSTT`, `FURN`, terreno e água. Animação, partículas,
personagens com skinning, física e colisão não entram neste plano, salvo quando forem indispensáveis
para a representação estática de um asset alcançável.

## Princípios de execução

1. Corrigir fidelidade antes de ativar otimizações de visibilidade.
2. Não aceitar fallback branco ou textura substituta como sucesso de integração.
3. Tratar caminhos e materiais no conversor; o runtime apenas carrega o contrato publicado.
4. Invalidar o cache sempre que a semântica do GLB ou KTX2 mudar.
5. Separar dependência obrigatória, recurso opcional e conteúdo fora do escopo nos relatórios.
6. Usar fixtures sintéticas no Git e assets reais somente na campanha local, sem redistribuí-los.

## Etapa 0 — Congelar o caso de reprodução

**Status de implementação:** implementada. Use `scripts/phase2-visual-baseline.ps1` para criar um
bundle estrito com metadados, hashes dos contratos, comando, logs, profiling, screenshot e inspeção
do mundo. O comando rejeita conjuntos sem manifest ou relatório de integração aprovado.

**Implementação**

1. Registrar commit, schema do manifest, load order, GPU/driver, resolução, worldspace, células,
   posição e orientação da câmera que reproduzem a cena problemática.
2. Capturar screenshot, log completo do runtime, `asset closure` e lista dos GLBs carregados.
3. Executar sem `--allow-incomplete-assets`; ausência obrigatória deve impedir o aceite.
4. Guardar no relatório apenas caminhos normalizados, hashes e metadados dos assets proprietários.

**Saída**

- caso reproduzível por um único comando;
- baseline visual WIP e inventário exato das falhas;
- nenhuma mudança posterior pode reduzir contagens escondendo assets.

## Etapa 1 — Adicionar diagnóstico por objeto e material

**Status de implementação:** implementada. O binário `world-inspect` aceita `--radius`,
`--reference` e `--output`, reproduz a consulta espacial do runtime e relaciona cada referência ao
GLB, transform, bounds, primitives, materiais glTF, modos de alpha, slots, URIs e texturas ausentes.
Metadados de shader NIF ainda não publicados são apresentados pela ausência do respectivo contrato,
sem inferência pelo nome. O runtime também contabiliza e registra falhas do modelo ou de
dependências recursivas.

**Implementação**

1. Estender `world-inspect` para localizar referências por worldspace/célula e imprimir FormID,
   modelo, transform final, bounds e material.
2. Registrar, por primitive, shader NIF, flags, slots de textura, URIs KTX2 e estado de carregamento.
3. Incluir no log de erro a cadeia `REFR -> base record -> NIF -> shape -> material -> textura`.
4. Adicionar um modo visual de diagnóstico que permita destacar bounds e distinguir material
   válido, textura obrigatória ausente e alpha ainda sem suporte.
5. Fazer os relatórios agregarem falhas por modelo e causa, sem repetir a mesma URI por instância.

**Arquivos principais**

- `crates/engine/src/bin/world-inspect.rs`;
- `crates/engine/src/streaming.rs`;
- `crates/converter/src/bin/asset-closure.rs`;
- `crates/converter/src/integration.rs`.

**Saída**

- cada objeto branco ou opaco da captura pode ser identificado sem inspeção manual do GLB;
- o relatório diferencia geometria, material, alpha, textura e carregamento.

## Etapa 2 — Fechar resolução e normalização de texturas

**Status de implementação:** implementada. O módulo `converter::asset_path` é o contrato canônico
para meshes, texturas e scripts. Archives são sobrepostos na ordem dos plugins, loose files vencem
por último e raízes fornecidas a `asset-closure-textures` devem ser informadas da menor para a maior
prioridade. Colisões dentro de uma camada, traversal, caminhos absolutos, caracteres de controle,
percent encoding inválido e extensões de textura não suportadas interrompem o processo. O schema do
conversor foi elevado para 8.

**Implementação**

1. Criar uma função canônica única para caminhos Bethesda: barras, caixa, prefixo `textures/`,
   espaços, extensão ausente e extensão DDS implícita.
2. Resolver caminhos contra a VFS final do load order, respeitando overrides de loose files e
   archives.
3. Detectar colisões depois da normalização e falhar com as duas origens, sem sobrescrita silenciosa.
4. Classificar cada slot como obrigatório, opcional ou não aplicável segundo o tipo de shader.
5. Fazer `asset-closure-textures` converter o fechamento completo e confirmar que toda URI publicada
   permanece dentro da raiz de assets.
6. Adicionar testes com caixa divergente, barras invertidas, espaços, extensão ausente, `.tga`/`.bmp`
   inválidos, caracteres de controle e colisão de nomes.

**Arquivos principais**

- `crates/converter/src/mesh.rs`;
- `crates/converter/src/texture.rs`;
- `crates/converter/src/archive/`;
- `crates/converter/src/bin/asset-closure-textures.rs`;
- `crates/converter/src/pipeline.rs`.

**Saída**

- zero texturas obrigatórias sem fonte no fechamento alcançável;
- zero colisões ou caminhos malformados ignorados;
- rerun do conversor reutiliza o cache e produz o mesmo relatório.

## Etapa 3 — Criar um contrato intermediário de material NIF

**Status de implementação:** implementada. O conversor extrai um contrato validado por shape antes
da exportação, resolve as referências explícitas de shader, texture set e alpha e registra exclusões
por motivo no `nif-audit`. A semântica dos slots vem do tipo e das flags do shader, nunca do nome do
arquivo. A publicação desse contrato como material glTF permanece na Etapa 4.
Como a correção dos layouts binários e dos sentinelas altera GLBs gerados, o schema do conversor foi
elevado para 9.

**Implementação**

1. Introduzir uma representação validada por shape contendo shader type, flags, cores, alpha,
   glossiness, emissive, double-sided e slots de textura com sua semântica.
2. Ler `BSLightingShaderProperty`, `BSEffectShaderProperty`, `BSShaderTextureSet`,
   `NiMaterialProperty`, `NiTexturingProperty` e `NiAlphaProperty` quando alcançáveis.
3. Consolidar propriedades herdadas na hierarquia sem perder overrides locais.
4. Definir regras explícitas para diffuse, normal, glow, specular, environment, cubemap e máscara.
5. Rejeitar valores não finitos e combinações impossíveis com arquivo, bloco e shape no erro.
6. Criar fixtures mínimas para material opaco, cutout, blend, emissive, double-sided e environment.

**Saída**

- toda primitive alcançável possui um material validado ou uma exclusão explícita;
- nenhuma decisão visual depende de heurística baseada apenas no nome do arquivo.

## Etapa 4 — Publicar materiais glTF/PBR corretos

**Implementação**

1. Mapear diffuse e cor do material para `baseColor`, preservando alpha.
2. Publicar normal maps como dados lineares; usar sRGB somente para texturas de cor.
3. Converter glossiness/specular para roughness/metallic com regra documentada e limitada.
4. Mapear glow para emissive, incluindo fator e força necessários.
5. Mapear `NiAlphaProperty` e shader flags para `OPAQUE`, `MASK` ou `BLEND`, incluindo cutoff.
6. Publicar `doubleSided` apenas quando requerido e preservar associação material/primitive.
7. Definir extensão/metadado próprio apenas para semântica Skyrim que o glTF core não represente.
8. Elevar o schema do conversor para invalidar todos os GLBs do contrato anterior.

**Arquivos principais**

- `crates/converter/src/mesh.rs`;
- `crates/converter/src/cache.rs`;
- `crates/converter/src/config.rs`.

**Saída**

- planos de folhagem, grades e recortes deixam de aparecer como placas brancas;
- superfícies opacas, emissivas e dupla-face correspondem às fixtures de referência;
- todos os GLBs passam na auditoria estrutural e de URIs.

**Status: implementada (schema 10).** A publicação deixa de reutilizar os materiais posicionais do
exportador e associa cada primitive ao bloco de shape de origem. O contrato cobre base color/alpha,
normal linear, roughness/specular limitado, emissive/strength, `OPAQUE`/`MASK`/`BLEND`, cutoff e
`doubleSided`; semânticas exclusivas do Skyrim ficam em `OPEN_SKYRIM_material`. As fixtures cobrem
as seis classes canônicas, exclusões e ordem de shapes invertida. A validação real de 11 NIFs de
mobiliário publicou 73 primitives (68 materiais e 5 exclusões explícitas), incluindo 5 cutouts,
14 blends, 12 emissivos e 2 double-sided, com zero falhas estruturais, de associação ou de URI.
Todo cache de schema anterior precisa ser reconvertido.

## Etapa 5 — Fechar conversão DDS para KTX2 por semântica

**Implementação**

1. Preservar alpha e mipmaps necessários para cutout e blend.
2. Escolher espaço de cor e formato de destino pela semântica do slot, não apenas pela extensão.
3. Validar BC1–BC7, cubemaps e demais variantes realmente alcançáveis.
4. Confirmar orientação, canais e intensidade de normal maps com uma fixture visual.
5. Verificar dimensões, níveis, formato, tamanho expandido e hash do KTX2 publicado.
6. Reconverter o fechamento depois do bump de schema e impedir sucesso parcial.

**Saída**

- conversão completa e determinística do fechamento de texturas;
- alpha e normal maps sobrevivem ao round-trip visual;
- nenhuma URI KTX2 obrigatória falta no disco.

**Status: implementada (schema 11).** O pipeline inverte as dependências publicadas pelos GLBs e
os slots de `texture_sets`/água para classificar cada DDS como cor sRGB, normal linear ou dado
linear; usos incompatíveis são erro. BC1–BC7, alpha, canais de normal e mipmaps autorados têm
fixtures de round-trip. Cada publicação é atômica e registra dimensões, níveis, faces/camadas,
formato, supercompressão, tamanho codificado/expandido e SHA-256. O fechamento de texturas v3
inclui a versão do schema e só passa quando toda URI obrigatória foi publicada e validada.

## Etapa 6 — Tornar o carregamento de material estrito no runtime

**Status de implementação:** implementada. O runtime só contabiliza uma instância depois do evento
de criação do mundo e da prontidão recursiva do `AssetServer`, valida meshes, materiais, imagens,
espaço de cor, sampler, alpha, emissive e culling, e inclui a cadeia de dependência e os FormIDs no
relatório de streaming. Falhas escondem a cena parcial; o fallback magenta exige
`--diagnostic-asset-fallbacks` e continua reprovando a aceitação. O cenário automático `materials`
exercita opaco, cutout, blend, emissive, double-sided e normal map e só captura o screenshot após o
warm-up e a validação completa.

**Implementação**

1. Aguardar a prontidão de mesh, material e imagens antes de contabilizar o asset como pronto.
2. Propagar falhas do `AssetServer` para o relatório de streaming com a cadeia de dependência.
3. Permitir fallback chamativo apenas no modo de diagnóstico; na aceitação, fallback é falha.
4. Validar `AlphaMode`, culling de faces, emissive e sampler carregados pelo Bevy.
5. Criar uma cena automática com os materiais canônicos e screenshot determinístico.

**Arquivos principais**

- `crates/engine/src/streaming.rs`;
- `crates/engine/src/metrics.rs`;
- `crates/engine/src/profiling.rs`.

**Saída**

- zero erro ou warning relevante de asset nos cenários reais;
- zero fallback branco na captura depois do warm-up;
- falha de uma textura obrigatória torna o cenário `rejected`.

Execute `scripts/phase2-acceptance.ps1 -Quick` para validar a fixture sem assets proprietários, ou
informe `-Assets <diretório-convertido>` para incluir os cenários reais. Os relatórios de aceitação
e profiling expõem contagens de assets pendentes/prontos, meshes, materiais e imagens validados,
falhas detalhadas e fallbacks diagnósticos.

## Etapa 7 — Corrigir terreno e água

**Status de implementação:** implementada (converter schema 13; cell cache 3). O cache agora
preserva explicitamente `BTXT` versus `ATXT`, lê o índice `u16` correto de cada overlay e rejeita
quadrantes, pesos, opacidades e payloads truncados. O runtime divide cada LAND nos quatro
quadrantes de 17×17 vértices, mantém UV global contínuo, VCLR independente, winding positivo e uma
paleta determinística de base mais cinco overlays. As quatro bordas são comparadas com células
residentes vizinhas e qualquer rachadura reprova a aceitação. Texturas de terreno e flow normals
são aguardadas e validadas com o espaço de cor correto antes da superfície ser considerada pronta.
A câmera de reflexão espelha posição e direção tanto acima quanto abaixo do plano e renderiza
somente a layer 0; água permanece na layer 1, impedindo reflexão recursiva. O cenário automático
`terrain-water` exercita seis layers, quatro quadrantes, VCLR, relevo, água animada, flow normal e
reflexão, gerando relatório e screenshot determinístico.

**Implementação**

1. Auditar por célula alturas, normais, cores, seis layers, pesos e costuras.
2. Corrigir orientação/escala dos UVs, seleção das layers e mistura da base no shader de terreno.
3. Validar winding e as quatro bordas entre células vizinhas.
4. Validar água acima/abaixo do plano, flow normal, movimento rápido e bordas de célula.
5. Impedir que a câmera refletida renderize a própria camada de água.
6. Adicionar fixtures e screenshots determinísticos de terreno e água.

**Arquivos principais**

- `crates/engine/src/shaders/terrain.wgsl`;
- `crates/engine/src/shaders/water.wgsl`;
- `crates/engine/src/render.rs`;
- `crates/engine/src/streaming.rs`;
- `crates/converter/src/esm/cell_cache.rs`.

**Saída**

- terreno sem cinza de fallback, rachaduras ou layers invertidas;
- seis camadas coerentes com os dados de origem;
- água estável, não recursiva e com flow normal quando disponível.

Execute `scripts/phase2-acceptance.ps1 -Quick` para rodar as fixtures `materials` e
`terrain-water`; forneça `-Assets <diretório-reconvertido>` para validar também as costuras e
dependências das células reais. Conjuntos anteriores ao schema 13/cache 3 são rejeitados.

## Etapa 8 — Validar transforms e bounds com os materiais finais

**Status de implementação:** implementada. Cada instância carregada agora valida o transform da
`REFR`, a equivalência de `WorldTransform`, todos os transforms locais e globais da hierarquia glTF
e a rotação normalizada/escala não singular. O runtime recompõe o AABB de cada primitive a partir
dos oito cantos do bounds local, aplica a hierarquia completa e compara o agregado aos bounds
publicados pelo conversor. Divergências ocultam a cena, registram `REFR`, base, célula, modelo,
bounds esperado/real e reprovam a aceitação. A fixture `transform-bounds` preserva um caso
estrutural sem conteúdo proprietário, com dois níveis, rotações em eixos diferentes e escalas não
uniformes. O gate rural/denso usa a mesma validação quando `-Assets` é fornecido ao script; a
revisão visual humana dessas capturas continua obrigatória para aprová-las.

**Implementação**

1. Repetir a comparação visual rural e densa com os materiais já corretos.
2. Investigar qualquer peça desmontada usando o diagnóstico da Etapa 1.
3. Validar rotação, escala não uniforme, hierarquia e os oito cantos dos bounds.
4. Registrar casos reais divergentes como fixtures estruturais sem incluir conteúdo proprietário.

**Saída**

- nenhuma peça deitada, desmontada ou deslocada nas cenas aprovadas;
- rotação da câmera não causa desaparecimento prematuro.

Execute `scripts/phase2-acceptance.ps1 -Quick` para a regressão estrutural automática ou forneça
`-Assets <diretório-reconvertido>` para repetir também as cenas rural e densa com materiais finais.
Os relatórios expõem instâncias, nós e bounds validados, além das divergências estruturais. O
schema 13 também separa usos sRGB e lineares da mesma imagem, ignora referências LAND nulas e
publica shapes explicitamente excluídos com material invisível, impedindo fallback branco.

## Etapa 9 — Reativar culling e provar o renderer final

**Status de implementação:** implementada. Todas as câmeras 3D do runtime usam `DepthPrepass` e
`OcclusionCulling`, inclusive a reflexão quando ativa. Uma ponte entre render-world e main-world
registra o suporte e o uso real de GPU preprocessing/culling, buffers de indirect draw, batch sets,
views de oclusão e pirâmides HZB; configurar o componente sem ativar esse caminho não satisfaz o
gate. A fixture `renderer` verifica um objeto diante do oclusor, outro totalmente atrás, objetos
laterais após rotações da câmera e escalas não uniformes, retornando a câmera à pose determinística
antes da captura. O cenário sintético foi isolado da reflexão de água, já coberta pela Etapa 7,
para medir somente o renderer de 250 mil instâncias. Em `release`, ele atingiu 122,78 FPS de média,
P95 de 10,12 ms e crescimento de 0,01 GiB, com GPU culling, indirect drawing, HZB, nove buffers de
fase e dois batch sets ativos. A área densa usa o mesmo gate quando assets reconvertidos são
fornecidos; sua aprovação visual/performance permanece dependente da campanha com conteúdo local.

**Implementação**

1. Reativar `OcclusionCulling` junto com `DepthPrepass` somente após as Etapas 2–8 estarem verdes.
2. Testar objetos à frente e atrás de oclusor, câmera em rotação e escalas não uniformes.
3. Confirmar GPU preprocessing, batching e indirect drawing no build `release`.
4. Medir 250 mil instâncias e a área densa; otimizar batches/buffers se os thresholds falharem.

**Saída**

- HZB/frustum ativos sem falsos negativos de visibilidade;
- média de FPS >= 60 e P95 <= 16,67 ms nos cenários exigidos.

Os bundles agora incluem `renderer.json`; qualquer benchmark gráfico sem GPU preprocessing,
culling, indirect draw, HZB ou batch sets ativos é reprovado. Execute
`scripts/phase2-acceptance.ps1 -Quick` para as fixtures e o cenário de 250 mil instâncias, ou
adicione `-Assets <diretório-reconvertido>` para medir também a área densa.

## Etapa 10 — Endurecer streaming e ciclo de vida

**Status:** implementada. A fixture automática `--streaming-fixture` executa travessia rápida,
teleportes, descarte stale, unload e rebasing repetido; as invariantes e o orçamento de commit são
gates do relatório e a campanha cobre entradas truncadas e encerramento determinístico do worker.

**Implementação**

1. Testar travessia rápida, teleporte, exterior/interior e rebasing repetido.
2. Verificar uma raiz por célula, uma requisição ativa por chave, descarte stale e unload completo.
3. Cobrir banco/cache/manifest truncados, assets ausentes e encerramento do worker.
4. Registrar orçamento de commit por frame e timeline no bundle.

**Saída**

- zero falhas, duplicações ou entidades órfãs;
- crescimento de memória <= 0,5 GiB no teste de estabilidade.

## Etapa 11 — Consolidar gates e executar a aceitação

**Status:** gates implementados; aceite real pendente. A campanha agora rejeita auditoria de assets,
logs, bundles, screenshots ou cenários incompletos e exige baseline no mesmo hardware e revisão
assinada para produzir `accepted`. A execução final ainda requer assets Skyrim legalmente
convertidos e revisão humana no hardware-alvo.

**Implementação**

1. Rodar format, todos os testes/targets, Clippy sem warnings e build `release`.
2. Integrar fechamento de assets, auditoria GLB/KTX2 e casos negativos ao preflight.
3. Fazer log relevante, fallback, screenshot ou cenário ausente resultar em `rejected`.
4. Rodar três repetições de synthetic, rural, dense, water, stress e stability.
5. Gerar baseline no mesmo hardware e repetir a campanha contra ele.
6. Preencher e assinar a revisão visual de todas as cenas.

**Saída**

- `acceptance-report.json` com veredito exatamente `accepted`;
- baseline disponível, nenhuma regressão fatal e bundle reproduzível;
- somente então atualizar README e roadmap para Phase 2 concluída.

## Ordem de dependências

```text
E0 reprodução
 └─> E1 diagnóstico
      └─> E2 caminhos/VFS
           └─> E3 IR de material
                └─> E4 glTF/PBR
                     └─> E5 DDS/KTX2
                          └─> E6 runtime estrito
                               ├─> E7 terreno/água
                               └─> E8 transforms/bounds finais
                                    └─> E9 culling/renderer
                                         └─> E10 streaming
                                              └─> E11 aceitação
```

## Sequência recomendada de pull requests

1. **Diagnóstico e baseline:** Etapas 0–1.
2. **Fechamento de caminhos e texturas:** Etapa 2.
3. **Contrato de material NIF:** Etapa 3.
4. **Exportação glTF e invalidação de cache:** Etapa 4.
5. **Conversão KTX2 por semântica:** Etapa 5.
6. **Runtime estrito e regressão de materiais:** Etapa 6.
7. **Terreno e água:** Etapa 7.
8. **Revisão final de transforms e bounds:** Etapa 8.
9. **Renderer, culling e performance:** Etapa 9.
10. **Streaming, gates e aceite:** Etapas 10–11.

Cada PR deve incluir fixtures, testes, atualização do schema quando aplicável e o delta do relatório
de fechamento. PRs não devem reduzir falhas alterando o escopo ou tornando dependências obrigatórias
opcionais sem uma regra documentada.

## Critério final

O plano está concluído somente quando os quatro cenários visuais não apresentarem fallback branco,
alpha incorreto, textura obrigatória ausente, terreno incoerente ou desaparecimento de geometria, e
quando a campanha completa no hardware-alvo produzir `accepted` com revisão visual assinada.

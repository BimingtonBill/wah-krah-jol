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

## Etapa 6 — Tornar o carregamento de material estrito no runtime

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

## Etapa 7 — Corrigir terreno e água

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

## Etapa 8 — Validar transforms e bounds com os materiais finais

**Implementação**

1. Repetir a comparação visual rural e densa com os materiais já corretos.
2. Investigar qualquer peça desmontada usando o diagnóstico da Etapa 1.
3. Validar rotação, escala não uniforme, hierarquia e os oito cantos dos bounds.
4. Registrar casos reais divergentes como fixtures estruturais sem incluir conteúdo proprietário.

**Saída**

- nenhuma peça deitada, desmontada ou deslocada nas cenas aprovadas;
- rotação da câmera não causa desaparecimento prematuro.

## Etapa 9 — Reativar culling e provar o renderer final

**Implementação**

1. Reativar `OcclusionCulling` junto com `DepthPrepass` somente após as Etapas 2–8 estarem verdes.
2. Testar objetos à frente e atrás de oclusor, câmera em rotação e escalas não uniformes.
3. Confirmar GPU preprocessing, batching e indirect drawing no build `release`.
4. Medir 250 mil instâncias e a área densa; otimizar batches/buffers se os thresholds falharem.

**Saída**

- HZB/frustum ativos sem falsos negativos de visibilidade;
- média de FPS >= 60 e P95 <= 16,67 ms nos cenários exigidos.

## Etapa 10 — Endurecer streaming e ciclo de vida

**Implementação**

1. Testar travessia rápida, teleporte, exterior/interior e rebasing repetido.
2. Verificar uma raiz por célula, uma requisição ativa por chave, descarte stale e unload completo.
3. Cobrir banco/cache/manifest truncados, assets ausentes e encerramento do worker.
4. Registrar orçamento de commit por frame e timeline no bundle.

**Saída**

- zero falhas, duplicações ou entidades órfãs;
- crescimento de memória <= 0,5 GiB no teste de estabilidade.

## Etapa 11 — Consolidar gates e executar a aceitação

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

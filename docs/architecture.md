# Arquitetura

- `apps/windows-host` enumera janelas capturáveis com Windows Graphics Capture e cria uma sessão de captura por janela selecionada. Cada sessão mantém apenas o quadro mais recente fora do transporte.
- `apps/macos-client` usa GPUI para o launcher e uma janela nativa por transmissão. A leitura e descompressão executam fora da thread da interface. O redimensionamento no Mac preserva a proporção da imagem.
- `packages/protocol` contém o formato do pareamento, as mensagens versionadas e o enquadramento do vídeo.

## Sessão e pareamento

O Mac valida o certificado do host distribuído no pareamento. A primeira mensagem dentro do TLS contém a versão, um token aleatório de 32 bytes e a operação. O host valida o token antes de enumerar, abrir ou capturar janelas. Use uma identidade por host. Qualquer pessoa com o arquivo de pareamento pode usar as operações implementadas. Para revogar o acesso, feche o host e remova a identidade em `%LOCALAPPDATA%\HyperSeamlessX\host-identity.json`. Ao iniciar o compartilhamento novamente, o host gera uma nova identidade. Exporte o novo pareamento para os clientes autorizados.

Mensagens JSON têm um prefixo de tamanho de 32 bits em ordem de rede. Após a confirmação de captura o fluxo contém pacotes binários. Cada quadro traz largura e altura em ordem de rede seguidas por BGRA comprimido em Zstandard. Um pacote vazio mantém a sessão ativa em janelas estáticas. Cada conexão serve uma operação. A captura exige um HWND e o PID correspondente, conferidos contra a lista atual.

## Validação em dois computadores

1. Rode `cargo test --locked -p protocol`.
2. Abra o host no desktop interativo do Windows, selecione o IP da LAN e exporte o pareamento.
3. Importe o arquivo no cliente Mac e abra Explorador e Bloco de Notas pelo launcher.
4. Abra as duas transmissões, altere o conteúdo no Windows e confira que as janelas independentes acompanham as alterações no Mac.
5. Feche uma transmissão no Mac e confirme que a outra permanece ativa. Feche a janela Windows que ainda está aberta e confira o estado de fim de transmissão no cliente.
6. Para medir a latência ponta a ponta, coloque os dois monitores no mesmo enquadramento de uma câmera de alta velocidade e compare a transição de um elemento visível no Windows e no Mac. Registre resolução, rede, FPS da câmera e distribuição das medições.

Em uma etapa posterior, substitua o transporte experimental do vídeo por WebRTC com H.264, mantenha a captura na GPU, adicione decodificação acelerada no Mac e meça as etapas antes de publicar benchmarks.

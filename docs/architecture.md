# Arquitetura

- `apps/windows-host` enumera janelas capturáveis com Windows Graphics Capture e cria uma sessão de captura por janela selecionada. Cada sessão mantém apenas o quadro mais recente fora do transporte.
- `apps/macos-client` usa GPUI para o launcher e uma janela nativa por transmissão. A leitura e descompressão executam fora da thread da interface. O primeiro quadro determina as dimensões iniciais, ajustadas à escala do monitor e à área útil disponível.
- `packages/protocol` contém os transportes TCP e WebSocket e as mensagens versionadas de autenticação, pareamento e vídeo.

## Descoberta local

Ao compartilhar por um endereço da LAN, o host anuncia o serviço `_hyper-seamless._tcp.local.` via DNS-SD/mDNS na interface escolhida. O modo de túnel local dispensa o anúncio. O launcher lista os anúncios com a mesma versão do protocolo e conserva o endereço manual para redes onde multicast não chega. O anúncio contém o nome do host, endereço, porta e versão. O pareamento autenticado continua obrigatório.

## Sessão e pareamento

O host abre uma janela de pareamento de 90 segundos mediante ação local. Ele aceita apenas um pedido pendente e aguarda aprovação do usuário no Windows. Os dois aplicativos derivam um código de oito dígitos do exportador de chaves TLS, vinculado ao handshake. As duas confirmações por interface precedem o uso da identidade pelo Mac. O certificado armazenado deve corresponder ao certificado da sessão de pareamento. Conexões posteriores validam o certificado armazenado e enviam o token aleatório de 32 bytes antes de solicitar operações.

O host usa uma identidade persistente. Para revogar o acesso de todos os clientes, feche o host e remova a identidade em `%LOCALAPPDATA%\HyperSeamlessX\host-identity.json`. Ao iniciar o compartilhamento novamente, o host gera uma identidade nova. Para parear após uma rotação, clique em **Esquecer host** no Mac e confirme o novo código.

O host aceita TLS diretamente na porta TCP ou dentro de mensagens WebSocket binárias em `/hyper-seamless-x`. Para túnel HTTP ou HTTPS, execute o agente de túnel no Windows e aponte para `127.0.0.1:47651` enquanto o host estiver no modo **Usar túnel local**. Para um agente TCP, cole o endereço TCP atribuído pelo serviço no cliente.

Mensagens JSON têm um prefixo de tamanho de 32 bits em ordem de rede. Após a confirmação de captura o fluxo contém pacotes binários. Cada quadro traz largura e altura em ordem de rede seguidas por BGRA comprimido em Zstandard. Um pacote vazio mantém a sessão ativa em janelas estáticas. Uma sessão TLS autenticada e vinculada à janela confirma eventos de entrada em ordem. O cliente reabre a sessão após inatividade, descarta eventos antigos da fila e evita reenviar comandos cujo sucesso seja incerto. A captura exige um HWND e o PID correspondente, conferidos contra a lista atual.

## Validação em dois computadores

1. Rode `cargo test --locked -p protocol`.
2. Abra o host no desktop interativo do Windows, selecione **Usar túnel local** ou um IP da LAN e clique em **Autorizar novo Mac**.
3. No Mac, use o endereço correspondente, compare os códigos e confirme o pareamento nos dois aplicativos.
4. Abra Explorador e Bloco de Notas pelo launcher. Mude o conteúdo no Windows e confira que as duas janelas independentes acompanham as alterações no Mac.
5. Confira o tamanho inicial, a proporção durante o redimensionamento e o aparecimento dos controles do Mac ao encostar na borda superior.
6. Feche uma transmissão no Mac e confirme que a outra permanece ativa. Clique em **Parar compartilhamento** no host e confira o estado de fim de transmissão no cliente.
7. Para medir a latência ponta a ponta, coloque os dois monitores no mesmo enquadramento de uma câmera de alta velocidade e compare a transição de um elemento visível no Windows e no Mac. Registre resolução, rede, FPS da câmera e distribuição das medições.

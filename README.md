# Hyper Seamless X

Aplicativos em Rust e GPUI para abrir janelas individuais de um PC Windows 11 no macOS. O host captura cada janela com Windows Graphics Capture. O Mac abre uma janela nativa independente para cada transmissão.

## Usar pela interface

Baixe a mesma versão do host para Windows x64 e do cliente para Mac Apple Silicon na página de [releases](https://github.com/JVini0166/hyper-seamless-x/releases). O executável Windows está sem assinatura e o aplicativo Mac usa assinatura ad hoc.

1. No Windows, abra o host e selecione o endereço da sua rede local. Se o Firewall solicitar acesso, autorize no perfil privado.
2. Clique em **Autorizar novo Mac**. No Mac, selecione o host em **Hosts na rede local** ou digite o endereço e clique em **Conectar**.
3. Confira o código exibido nos dois computadores. Clique em **Aprovar Mac** no Windows e confirme a comparação no Mac. O cliente guarda a identidade para as próximas conexões.
4. Clique em uma janela para abrir a transmissão. Ela abre com a proporção do quadro capturado e é reduzida se ultrapassar a área útil da tela.
5. Passe o cursor pela borda superior da transmissão para revelar os três botões nativos do macOS. O conteúdo capturado preserva a aparência do aplicativo Windows.
6. No Windows, clique em **Parar compartilhamento** para encerrar as transmissões.

## Endereço de conexão

- IP ou nome na rede local, como `192.168.1.20` ou `pc.local:47651`.
- Endereço TCP, como `tcp://0.tcp.ngrok.io:12345`.
- Link HTTP ou HTTPS de um túnel que aceite WebSocket, como `https://exemplo.ngrok.app`.

Para um túnel, selecione **Usar túnel local** no host Windows. Configure o túnel no mesmo Windows para encaminhar para `127.0.0.1:47651`. Cole no Mac o endereço público exibido pelo serviço. O cliente usa WebSocket para URLs HTTP/HTTPS e preserva o TLS entre os aplicativos dentro da conexão do túnel.

O túnel é opcional. O uso direto na LAN dispensa serviços externos. Consulte a documentação do ngrok para [endpoints HTTP](https://ngrok.com/docs/gateway/endpoints/http) e [endpoints TCP](https://ngrok.com/docs/gateway/endpoints/tcp). O ngrok pode aplicar limites do plano e exigir um método de pagamento para habilitar endpoints TCP no plano gratuito.

## Compilar

Use Rust estável e o toolchain MSVC com as ferramentas de build C++ do Visual Studio no Windows. No Mac, use o Xcode com as ferramentas de linha de comando selecionadas.

- Windows: execute `scripts/build-windows.ps1`. O executável fica em `target/release/windows-host.exe`.
- macOS: execute `scripts/package-macos.sh`. O aplicativo e o ZIP ficam em `target/dist`.
- Protocolo: execute `cargo test --locked -p protocol` em qualquer uma das plataformas.

## Estado

O fluxo de vídeo experimental usa BGRA com Zstandard por TCP ou WebSocket e captura limitada a cerca de 30 quadros por segundo. A transmissão aceita cliques esquerdo e direito quando a janela pode receber foco no Windows. Com a janela em primeiro plano, aceita texto e teclas de navegação. A rolagem do trackpad é encaminhada nos dois eixos. Após uma interrupção de rede, janelas abertas tentam retomar a transmissão automaticamente. Arraste, atalhos com modificadores, sincronização automática da geometria das janelas, bandeja do Windows e o pipeline WebRTC/H.264 com aceleração por hardware ainda estão pendentes. As metas de latência, resolução e FPS exigem medições.

[Arquitetura e procedimento de validação](docs/architecture.md).

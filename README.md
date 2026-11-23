# Hyper Seamless X

Aplicativos em Rust e GPUI para abrir janelas individuais de um PC Windows 11 no macOS. O host captura cada janela com Windows Graphics Capture. O Mac abre uma janela nativa independente para cada transmissão.

## Usar pela interface

Baixe a mesma versão do host para Windows x64 e do cliente para Mac Apple Silicon na página de [releases](https://github.com/JVini0166/hyper-seamless-x/releases). O executável Windows está sem assinatura e o aplicativo Mac usa assinatura ad hoc.

1. No Windows, abra o host e selecione o endereço da sua rede local. Se o Firewall solicitar acesso, autorize no perfil privado.
2. Clique em **Autorizar novo Mac**. No Mac, selecione o host em **Hosts na rede local** ou digite o endereço e clique em **Conectar**.
3. Confira o código exibido nos dois computadores. Clique em **Aprovar Mac** no Windows e confirme a comparação no Mac. O cliente guarda a identidade para as próximas conexões. Se o IP ou o link mudar, digite o novo endereço. O pareamento é reaproveitado quando o host confirma a mesma identidade.
4. Clique em uma janela para abrir a transmissão ou trazer a janela já aberta para frente. Se estiver minimizada no Windows, o host tenta restaurá-la antes de capturar. Ela abre nas dimensões em pixels do quadro capturado, incluindo em telas Retina, e mantém a proporção ao redimensionar. Quando as dimensões da captura mudam no Windows, a janela acompanha a mudança no Mac em modo janela. Ao terminar de redimensioná-la pela borda no Mac, o cliente solicita o novo tamanho ao Windows. Se a janela for maior que a área útil, parte dela pode ficar fora da tela.
5. Passe o cursor pela borda superior da transmissão para revelar os três botões nativos do macOS. O conteúdo capturado preserva a aparência do aplicativo Windows.
6. No Windows, clique em **Parar compartilhamento** para encerrar as transmissões.

O launcher do Mac mostra atalhos para o Explorador, o Bloco de Notas e o VS Code quando o host encontra os aplicativos nas pastas padrão do Windows. Se o atalho abrir uma única nova janela do aplicativo, ela aparece automaticamente no Mac. A lista é atualizada enquanto o launcher está em primeiro plano. Ative **Diagnósticos nas novas janelas** para exibir FPS recebidos, bitrate e tempo de decodificação na próxima transmissão. Com WebRTC, a janela também mostra a latência de ida e volta e a perda de pacotes quando há amostras da rede.

## Endereço de conexão

- IP ou nome na rede local, como `192.168.1.20` ou `pc.local:47651`.
- Endereço TCP, como `tcp://0.tcp.ngrok.io:12345`.
- Link HTTP ou HTTPS de um túnel que aceite WebSocket, como `https://exemplo.ngrok.app`.

Para um túnel, selecione **Usar túnel local** no host Windows. Se o ngrok já estiver instalado, autenticado e no PATH do Windows, clique em **Iniciar túnel público**. O host abre um endpoint HTTP, copia o link e encerra o agente que iniciou ao parar o compartilhamento. Para usar um agente externo, configure o túnel no mesmo Windows para encaminhar para `127.0.0.1:47651`. Cole no Mac o endereço público exibido pelo serviço. Se o agente ngrok já estiver rodando no Windows com o endereço de inspeção padrão, clique em **Copiar link do ngrok** para copiar o link do endpoint correspondente. O cliente usa WebSocket para URLs HTTP/HTTPS e preserva o TLS entre os aplicativos dentro da conexão do túnel.

O túnel é opcional. O uso direto na LAN dispensa serviços externos. Consulte a documentação do ngrok para [endpoints HTTP](https://ngrok.com/docs/gateway/endpoints/http) e [endpoints TCP](https://ngrok.com/docs/gateway/endpoints/tcp). O ngrok pode aplicar limites do plano e exigir um método de pagamento para habilitar endpoints TCP no plano gratuito.

## Compilar

Use Rust estável e o toolchain MSVC com as ferramentas de build C++ do Visual Studio no Windows. No Mac, use o Xcode com as ferramentas de linha de comando selecionadas.

- Windows: execute `scripts/build-windows.ps1`. O executável fica em `target/release/windows-host.exe`.
- macOS: execute `scripts/package-macos.sh`. O aplicativo e o ZIP ficam em `target/dist`.
- Protocolo: execute `cargo test --locked -p protocol` em qualquer uma das plataformas.

## Estado

Na LAN, o vídeo H.264 tenta WebRTC por UDP com sinalização autenticada. Quando UDP não está disponível ou o endereço é um túnel, o cliente usa TCP ou WebSocket. O host tenta codificar com NVENC ou Media Foundation e usa BGRA com Zstandard como fallback. A captura mira até 60 quadros por segundo para a janela em primeiro plano e cerca de 15 para janelas em segundo plano. O host também ajusta a frequência de captura e o bitrate H.264 conforme a estimativa de banda do WebRTC. A transmissão aceita cliques esquerdo, direito e do meio quando a janela pode receber foco no Windows. Com a janela em primeiro plano, aceita texto, teclas de navegação e atalhos de edição com ⌘ ou Ctrl (selecionar tudo, copiar, recortar, colar, desfazer e refazer). Os atalhos de copiar e colar usam a área de transferência do Windows. A rolagem do trackpad é encaminhada nos dois eixos. Após uma interrupção de rede, janelas abertas tentam retomar a transmissão automaticamente. O arraste dentro de uma janela é enviado quando o botão é solto. Arraste em tempo real, outros atalhos com modificadores, sincronização de posição, e bandeja do Windows ainda estão pendentes. As metas de latência, resolução e FPS exigem medições.

[Arquitetura e procedimento de validação](docs/architecture.md).

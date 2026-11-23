# Hyper Seamless X

Aplicativos em Rust e GPUI para abrir janelas individuais de um PC Windows 11 no macOS. O host captura cada janela com Windows Graphics Capture. O Mac abre uma janela nativa independente para cada transmissão.

## Usar pela interface

Baixe o host para Windows x64 e o cliente para Mac Apple Silicon na página de [releases](https://github.com/JVini0166/hyper-seamless-x/releases). Os pacotes desta prévia têm assinatura ad hoc no Mac e executável sem assinatura no Windows.

1. Abra o host no Windows e selecione o endereço da rede local que o Mac usa. Se o Windows solicitar acesso à rede, selecione o perfil privado.
2. Clique em **Exportar pareamento** e transfira o arquivo JSON ao Mac por um canal privado. O arquivo concede acesso ao Windows enquanto a identidade do host estiver ativa.
3. Abra **Hyper Seamless X.app** no Mac e clique em **Importar pareamento**. Selecione o JSON. O aplicativo salva o pareamento e lista as janelas disponíveis.
4. Clique em uma janela para abrir a transmissão. Use os botões do launcher para abrir o Explorador ou o Bloco de Notas e atualizar a lista. A barra da transmissão permite restaurar, minimizar, maximizar e fechar a janela no Windows.
5. No host, clique em **Parar compartilhamento** para encerrar as transmissões.

Se o endereço do Windows mudar, selecione o novo endereço e exporte outro arquivo de pareamento.

## Compilar

Use Rust estável e o toolchain MSVC com as ferramentas de build C++ do Visual Studio no Windows. No Mac, use o Xcode com as ferramentas de linha de comando selecionadas.

- Windows: execute `scripts/build-windows.ps1`. O executável fica em `target/release/windows-host.exe`.
- macOS: execute `scripts/package-macos.sh`. O aplicativo e o ZIP ficam em `target/dist`.
- Protocolo: execute `cargo test --locked -p protocol` em qualquer uma das plataformas.

## Estado

O fluxo de vídeo experimental usa BGRA com Zstandard por TCP e captura limitada a cerca de 30 quadros por segundo. Ainda faltam mouse e teclado remotos, sincronização automática da geometria das janelas, descoberta mDNS, reconexão, bandeja do Windows e o pipeline WebRTC/H.264 com aceleração por hardware. As metas de latência, resolução e FPS ainda exigem medições.

[Arquitetura e procedimento de validação](docs/architecture.md).

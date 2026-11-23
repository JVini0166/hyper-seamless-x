# Hyper Seamless X

Aplicativo experimental em Rust para visualizar janelas individuais de um PC Windows 11 no macOS. O host usa Windows Graphics Capture e o launcher GPUI abre cada transmissão em uma janela nativa independente do Mac.

## Executar

Instale Rust estável nos dois computadores. Para compilar no Windows use o toolchain MSVC com as ferramentas de build C++ do Visual Studio. No Mac use o Xcode com as ferramentas de linha de comando selecionadas.

No PowerShell do Windows, em uma sessão gráfica ativa, execute a partir da raiz do repositório:

```powershell
.\scripts\build-windows.ps1
.\target\release\windows-host.exe init "$env:LOCALAPPDATA\HyperSeamlessX"
.\target\release\windows-host.exe serve "192.168.1.20:47651" "$env:LOCALAPPDATA\HyperSeamlessX"
```

Substitua o IP pelo endereço local do Windows. Se o firewall solicitar acesso, use o perfil da rede privada. Transfira somente o arquivo `pairing.json` ao Mac por um canal privado e mantenha os dois arquivos de identidade fora do repositório.

No terminal do Mac:

```sh
chmod 600 ~/Downloads/pairing.json
./scripts/run-macos.sh 192.168.1.20:47651 ~/Downloads/pairing.json
```

Clique numa janela para visualizar a transmissão. O launcher abre o Explorador ou o Bloco de Notas e oferece atualização manual da lista. Cada janela do Mac pode ser movida, redimensionada ou fechada independentemente. A barra da transmissão permite restaurar, minimizar, maximizar e fechar a janela no Windows.

## Estado

Esta versão implementa descoberta manual de janelas, abertura de dois aplicativos, pareamento por arquivo, TLS e visualização de múltiplas janelas. O fluxo de vídeo experimental usa BGRA com Zstandard por TCP e captura limitada a cerca de 30 quadros por segundo. Use janelas pequenas durante a validação em Wi-Fi. Ainda faltam mouse e teclado remotos, sincronização automática do estado das janelas, descoberta mDNS, reconexão, bandeja do Windows e o pipeline WebRTC/H.264 com codificação e decodificação por hardware. As metas de latência, resolução e FPS ainda exigem medições.

[Arquitetura e procedimento de validação](docs/architecture.md).

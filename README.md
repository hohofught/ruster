# ruster

ruster is a Windows local translation proxy that connects desktop apps to Gemini WebView, ChatGPT WebView, or Gemini CLI through OpenAI-compatible, Gemini-compatible, and simple Custom API endpoints.

## English Quick Start

### Features

- Desktop GUI with tray mode, logs, runtime status, and usage statistics
- Backend selection between Gemini WebView, ChatGPT WebView, and Gemini CLI
- OpenAI-compatible API: `/v1/chat/completions`, `/v1/responses`, `/v1/completions`, `/v1/models`
- Gemini-compatible API: `/models`, `/v1beta/models`, `/v1beta/models/{model}:generateContent`
- Simple Custom API endpoints: `/`, `/translate`, `/custom/{preset}`
- ivLyrics prompt handling for translation, pronunciation, study, and quiz requests
- Editable prompt configuration saved as `prompts.json`

### Requirements

- Windows 10/11
- Microsoft Edge WebView2 Runtime for WebView backends
- Node.js LTS and `@google/gemini-cli` when using Gemini CLI mode

### Run

1. Download or build `ruster.exe`.
2. Start the app and choose `Gemini WebView`, `Gemini CLI`, or `ChatGPT WebView`.
3. Check the local base URL. The default is `http://localhost:5000`.
4. Copy the local API key from the GUI, or disable local API key authentication for trusted local-only setups.
5. Enable tray mode if you want the backend to keep running without a console window. When started from the GUI, tray mode relaunches the backend as a detached tray process.

Headless examples:

```powershell
.\ruster.exe --headless --mode=webview
.\ruster.exe --headless --mode=chatgpt
.\ruster.exe --headless --mode=cli
```

### API Examples

OpenAI-compatible:

```http
POST http://127.0.0.1:5000/v1/chat/completions
Authorization: Bearer <local-api-key>
Content-Type: application/json
```

Gemini-compatible:

```http
POST http://127.0.0.1:5000/v1beta/models/gemini-2.5-flash:generateContent?key=<local-api-key>
Content-Type: application/json
```

Custom API:

```http
POST http://127.0.0.1:5000/translate
Authorization: Bearer <local-api-key>
Content-Type: application/json
```

```json
{
  "text": "Text to translate",
  "source": "auto",
  "target": "ko"
}
```

The default Custom API response shape is:

```json
{
  "result": "Translated text",
  "errorMessage": "",
  "errorCode": "0"
}
```

For ivLyrics, use the OpenAI-compatible endpoint when possible:

- Base URL: `http://127.0.0.1:5000/v1`
- API Key: the local API key shown in ruster
- Model: any model name accepted by ivLyrics, for example `gpt-4o-mini`
- Chat Completions endpoint: `/chat/completions`

ruster detects ivLyrics requests and rewrites translation/pronunciation prompts when useful. If `ivLyrics study/quiz CLI fast lane` is enabled, detected study, quiz, expression, summary, and line-study prompts are forwarded as the raw prompt received from the client. With the fast wrapper enabled, that path posts directly to Gemini Code Assist with a model-compatible `thinkingConfig` (`thinkingLevel` for Gemini 3 models, `thinkingBudget` for Gemini 2.5 models) and uses fail-fast request/empty-response attempts for lower latency. The path bypasses proxy deduplication and Gemini request gates for maximum throughput. When a WebView backend starts, ruster preloads the study CLI path in the background so the first study request does not pay the wrapper/auth/setup cost. WebView is not parallelized; it is used only as a serialized fallback for limit-like CLI failures when a WebView backend is available.

### Data Location

Settings, prompts, WebView profiles, and usage statistics are stored under `%LOCALAPPDATA%\ruster` by default. To use portable mode, place `ruster.portable` or `settings.json` next to `ruster.exe`; writable data will then be stored beside the executable.

### Libraries Used

- Runtime and HTTP: `tokio`, `axum`, `reqwest`
- GUI and window integration: `eframe`/`egui`, `raw-window-handle`
- Windows integration: `windows`, `webview2-com`
- Serialization and data: `serde`, `serde_json`, `chrono`, `dirs`, `uuid`
- Parsing and utilities: `regex`, `url`, `urlencoding`, `sha2`
- Errors and synchronization: `anyhow`, `thiserror`, `parking_lot`

## 한국어

ruster는 Gemini WebView, Gemini CLI, ChatGPT WebView를 로컬 번역 엔진으로 묶어 주는 Windows용 번역 프록시입니다. 외부 앱에서는 OpenAI 호환 API, Gemini 호환 API, 또는 단순 Custom API 형식으로 `http://localhost:5000`에 요청하면 됩니다.

## 주요 기능

- GUI 대시보드, 트레이 실행, 로그/통계 창
- Gemini WebView, ChatGPT WebView, Gemini CLI 백엔드 선택
- OpenAI 호환 API: `/v1/chat/completions`, `/v1/responses`, `/v1/completions`, `/v1/models`
- Gemini 호환 API: `/models`, `/v1beta/models`, `/v1beta/models/{model}:generateContent`
- Custom API 호환 엔드포인트: `/`, `/translate`, `/custom/{preset}`
- ivLyrics 번역/발음/학습/퀴즈 프롬프트 보정
- GUI에서 프롬프트 직접 수정 및 `prompts.json` 저장

## 설치

### 릴리스 실행 파일 사용

1. 배포된 `ruster.exe`를 원하는 폴더에 둡니다.
2. Windows WebView2 Runtime이 없다면 Microsoft Edge WebView2 Runtime을 설치합니다.
3. Gemini CLI 모드를 쓸 경우 Node.js LTS와 `@google/gemini-cli`가 필요합니다. GUI의 `CLI 초기설정` 버튼으로 설치/로그인 흐름을 실행할 수 있습니다.
4. `ruster.exe`를 실행하고 시작 화면에서 사용할 번역 모드를 선택합니다.

### 소스에서 빌드

```powershell
cargo build --release
.\target\release\ruster.exe
```

디버그 빌드는 아래처럼 실행합니다.

```powershell
cargo build
.\target\debug\ruster.exe
```

### 데이터 저장 위치

기본 설정/프롬프트/통계는 `%LOCALAPPDATA%\ruster`에 저장됩니다. 실행 파일이 있는 폴더에 `ruster.portable` 파일을 만들거나 `settings.json`을 함께 두면, 쓰기 권한이 있는 경우 포터블 모드로 같은 폴더에 데이터를 저장합니다.

## 초기 설정

1. 시작 화면에서 `Gemini WebView`, `Gemini CLI`, `ChatGPT WebView` 중 하나를 선택합니다.
2. `서버 / 프록시`에서 `Base URL`을 확인합니다. 기본값은 `http://localhost:5000`입니다.
3. 외부 앱에서 호출할 경우 `로컬 API 키`를 복사합니다. 인증을 끄려면 `로컬 API 키 인증 요구`를 비활성화합니다.
4. ivLyrics를 쓸 경우 `프롬프트 / ivLyrics`에서 학습/퀴즈 CLI fast lane과 프롬프트 내용을 조정합니다.
5. 트레이 모드가 필요하면 `트레이 모드로 실행`을 켭니다. GUI에서 시작할 때는 콘솔 없는 트레이 백엔드로 재실행되며, 로그/종료는 트레이 메뉴에서 처리합니다.

헤드리스 실행 예시는 아래와 같습니다.

```powershell
.\ruster.exe --headless --mode=webview
.\ruster.exe --headless --mode=chatgpt
.\ruster.exe --headless --mode=cli
```

## 로컬 API

인증이 켜져 있으면 다음 중 하나로 로컬 API 키를 전달합니다.

- `Authorization: Bearer <local-api-key>`
- `x-api-key: <local-api-key>`
- `api-key: <local-api-key>`
- `x-goog-api-key: <local-api-key>`
- 쿼리 문자열 `?key=<local-api-key>` 또는 `?api_key=<local-api-key>`

### OpenAI 호환 호출

```http
POST http://127.0.0.1:5000/v1/chat/completions
Authorization: Bearer <local-api-key>
Content-Type: application/json
```

```json
{
  "model": "gpt-4o-mini",
  "messages": [
    { "role": "user", "content": "Translate to Korean: hello" }
  ]
}
```

모델명은 외부 앱 호환용으로 받으며, 실제 처리 백엔드는 ruster에서 선택한 모드와 설정을 따릅니다.

### Gemini 호환 호출

```http
POST http://127.0.0.1:5000/v1beta/models/gemini-2.5-flash:generateContent?key=<local-api-key>
Content-Type: application/json
```

```json
{
  "contents": [
    {
      "role": "user",
      "parts": [{ "text": "Translate to Korean: hello" }]
    }
  ]
}
```

### Custom API 호환 호출

```http
POST http://127.0.0.1:5000/translate
Authorization: Bearer <local-api-key>
Content-Type: application/json
```

```json
{
  "text": "翻訳する文章",
  "source": "ja",
  "target": "ko"
}
```

응답은 기본적으로 아래 형식입니다.

```json
{
  "result": "번역 결과",
  "errorMessage": "",
  "errorCode": "0"
}
```

요청 본문은 `text`, `prompt`, `q`, `input` 필드를 우선 사용합니다. 일반 문자열 본문, OpenAI `messages`, Gemini `contents.parts.text` 형태도 추출합니다.

## ivLyrics 적용법

ivLyrics에서 OpenAI 호환 번역기를 선택할 수 있다면 아래처럼 설정하는 것이 가장 간단합니다.

- Base URL: `http://127.0.0.1:5000/v1`
- API Key: ruster의 `로컬 API 키`
- Model: `gpt-4o-mini` 또는 ivLyrics가 허용하는 임의 모델명
- Chat Completions endpoint: `/chat/completions`

Gemini 호환 번역기로 연결해야 한다면 아래 주소를 사용합니다.

```text
http://127.0.0.1:5000/v1beta/models/gemini-2.5-flash:generateContent?key=<local-api-key>
```

ivLyrics가 단순 Custom API만 지원하는 경우:

- URL: `http://127.0.0.1:5000/translate`
- Method: `POST`
- Header: `Content-Type: application/json`
- Header: `Authorization: Bearer <local-api-key>`
- Body: `{"text":"<ivLyrics 원문 변수>","source":"auto","target":"ko"}`
- Result path: `result`

ivLyrics 쪽 변수명은 버전마다 다를 수 있으므로, ivLyrics가 제공하는 원문/가사 변수를 위 예시의 `<ivLyrics 원문 변수>` 위치에 넣으면 됩니다.

ruster는 ivLyrics 요청을 감지하면 번역/발음 프롬프트는 필요할 때 보정합니다. GUI의 `프롬프트 편집`에서 기본 규칙을 수정할 수 있고, 저장하면 데이터 폴더의 `prompts.json`에 반영됩니다. `ivLyrics 학습/퀴즈 CLI fast lane`을 켜면 학습, 퀴즈, 표현, 요약, 라인 학습 요청은 클라이언트에서 받은 원본 prompt 그대로 즉시 전달합니다. Fast wrapper가 켜져 있으면 Gemini Code Assist로 직접 POST하며, Gemini 3 계열은 `thinkingLevel`, Gemini 2.5 계열은 `thinkingBudget`을 모델 호환 형태로 붙입니다. 이 경로는 proxy dedup과 Gemini 요청 게이트를 우회하고, 내부 HTTP/빈 응답 재시도를 1회로 줄여 지연을 최소화합니다. WebView 백엔드가 시작되면 학습 CLI 경로를 백그라운드에서 preload해 첫 학습 요청의 wrapper/auth/setup 지연을 줄이고, CLI 요청 한도/429류 실패가 날 때만 WebView 모드에서 직렬 WebView fallback을 사용합니다.

## Custom API 적용법

프로그램 화면에서는 이 기능을 `호환 API`로 표시하지만, MORT의 Custom API 연결에는 그대로 사용할 수 있습니다.

Custom API 설정 예시:

- URL: `http://127.0.0.1:5000/translate`
- Method: `POST`
- Content-Type: `application/json`
- 인증 헤더: `Authorization: Bearer <local-api-key>`
- 요청 본문: `{"text":"<MORT OCR/원문 변수>","source":"auto","target":"ko"}`
- 결과 경로: `result`

MORT에서 헤더 설정이 불편하면 URL에 키를 붙여도 됩니다.

```text
http://127.0.0.1:5000/translate?key=<local-api-key>
```

로컬 키 인증을 쓰지 않을 환경이라면 ruster의 `서버 / 프록시`에서 `로컬 API 키 인증 요구`를 끄면 됩니다. 단, 외부 네트워크에서 접근 가능한 주소로 열어 둔 경우에는 인증을 끄지 않는 것이 좋습니다.

### Custom API 프리셋

`/custom/{preset}` 경로를 쓰면 데이터 폴더의 `CustomApi` 디렉터리에 있는 프리셋을 적용합니다.

기본 위치:

```text
%LOCALAPPDATA%\ruster\CustomApi
```

포터블 모드:

```text
<ruster.exe 폴더>\CustomApi
```

예시 파일: `CustomApi\game-ja-ko.json`

```json
{
  "Name": "game-ja-ko",
  "Mode": "translate",
  "RequestTemplate": "다음 일본어 게임 대사를 자연스러운 한국어로 번역해.\n\n{OCR_TEXT}",
  "ResponseTemplate": "\"result\":\"{RESULT_TEXT}\",\"errorMessage\":\"\",\"errorCode\":\"0\"",
  "TimeoutSeconds": 60
}
```

호출 URL:

```text
http://127.0.0.1:5000/custom/game-ja-ko
```

`Mode` 값은 `translate` 또는 `raw`를 사용할 수 있습니다. `translate`는 ruster의 번역 래핑을 적용하고, `raw`는 `RequestTemplate`으로 만든 프롬프트를 그대로 백엔드에 보냅니다. 프리셋 목록은 `GET /custom/presets`로 확인할 수 있습니다.

현재 로컬 수신 프리셋에서 실제로 영향을 주는 필드는 `Name`, `RequestTemplate`, `ResponseTemplate`, `Mode`, `TimeoutSeconds`입니다. `Url`, `Method`, `Headers`, `ResultPath` 필드는 기존 설정 파일 호환을 위해 읽을 수 있지만, 이 경로에서 외부 API로 재전송하는 용도로 사용하지 않습니다.

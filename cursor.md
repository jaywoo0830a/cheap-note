필기 앱에서 팬 모양 커서를 "저수준·고성능"으로 구현하려면, **`NSCursor`의 고수준 경로(`resetCursorRects`나 빈번한 `set`)를 피하고, 시스템 커서는 투명하게 숨긴 뒤 Metal 렌더 패스에서 직접 팬을 그리는 전략**이 가장 확실합니다. 이 방식은 120Hz ProMotion 환경에서 커서 렌더링이 프레임 페이싱을 방해하지 않도록 완전히 분리할 수 있습니다.

### ⚠️ 먼저, 왜 고수준 경로가 느린가

`NSCursor`를 뷰의 커서 사각형에 연결하는 `resetCursorRects` 방식은 커서가 바뀔 때마다 사각형을 무효화하고 다음 이벤트 루프에서 다시 만드는데, 이 과정이 **1ms 이상 걸릴 수 있습니다**. `NSCursor.set`은 이보다 즉시 실행되지만, Flutter 사례에서 보듯 **매 디스플레이 사이클마다 호출되면 CPU 사용량이 85~90%까지 치솟을 정도로 비용이 큽니다**.

또한 전체 화면 Metal 레이어에서 `NSCursor.hide()`로 커서를 숨기면 **커서 전환 중에 디스플레이 리프레시를 놓치는 VSync 프레임 드롭**이 발생할 수 있습니다. 이 문제의 해결책으로 제시된 것이 **캐시된 투명 커서(transparent NSCursor)** 로 숨기는 방식입니다. 이렇게 하면 하드웨어 커서 평면이 활성 상태로 유지되어 추가적인 프레임 드롭이 사라집니다.

### 🎯 권장 전략: 투명 커서 + Metal 자체 렌더링

**핵심 아이디어**: 시스템 커서는 **투명한 `NSCursor`로 대체**하여 하드웨어 커서 평면의 동기화 비용을 없애고, 팬 모양은 **이미 구동 중인 Metal 파이프라인에서 스프라이트로 그려** 커서 모양 변경과 애니메이션을 프레임 예산 안에서 완전히 통제하는 것입니다. Stack Overflow에서도 정확한 1픽셀 제어를 위해 **시스템 커서를 숨기고 직접 그리는 방식**이 사용된다고 언급됩니다.

#### 1단계: 투명 커서 준비 및 적용

`NSCursor.hide()` 대신 **1x1 투명 이미지로 만든 `NSCursor`를 `set`** 합니다. `set`은 한 번만 호출하고, 이후 커서 모양 전환은 Metal 텍스처 교체로 처리합니다.

```swift
// 1x1 투명 이미지 생성
let transparentImage = NSImage(size: NSSize(width: 1, height: 1))
transparentImage.lockFocus()
NSColor.clear.setFill()
NSRect(x: 0, y: 0, width: 1, height: 1).fill()
transparentImage.unlockFocus()

let transparentCursor = NSCursor(image: transparentImage, hotSpot: .zero)

// 뷰가 활성화될 때 한 번만 설정
override func viewDidMoveToWindow() {
    super.viewDidMoveToWindow()
    transparentCursor.set()
}
```

`CGDisplayHideCursor`도 저수준 선택지이지만, 카운터 기반이라 복원 시점을 놓치기 쉽고 VSync 동기화 경로를 건드릴 수 있습니다. 투명 `NSCursor`가 더 안전하고 예측 가능합니다.

#### 2단계: Metal 렌더 패스에 팬 스프라이트 추가

필기 앱의 `CAMetalDisplayLink` 콜백(또는 `MTKView`의 draw 루프)에서, 마우스의 현재 위치를 받아 팬 모양을 그리는 드로우 콜을 추가합니다. 팬 모양은 **SF Symbol(`hand.point.up.left`)을 텍스처 아틀라스로 미리 래스터화**하거나, 커스텀 아트워크를 `MTLTexture`로 업로드해 사용합니다.

```swift
// 매 프레임: 마우스 위치에 팬 스프라이트를 그리는 렌더 패스
func renderCursorSprite(encoder: MTLRenderCommandEncoder,
                        mouseLocation: CGPoint,
                        isDragging: Bool) {
    // 상태에 따라 텍스처 교체 (열린 손 / 닫힌 손)
    let cursorTexture = isDragging ? closedHandTexture : openHandTexture

    var vertices = quadVertices(at: mouseLocation, size: cursorTexture.size)
    encoder.setVertexBytes(&vertices, length: MemoryLayout<Vertex>.stride * vertices.count, index: 0)
    encoder.setFragmentTexture(cursorTexture, index: 0)
    encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
}
```

이 스프라이트는 **배경/잉크 레이어 위에, 하지만 UI 레이어 아래**에 그려지도록 렌더 순서를 조정합니다. 팬 커서는 필기 캔버스 위에서만 보여야 하므로, 캔버스 뷰의 좌표계로 위치를 변환해 그립니다.

#### 3단계: ProMotion과의 동기화

커서 스프라이트는 `CAMetalDisplayLink`의 콜백에서 **매 디스플레이 리프레시마다** 마우스 위치를 갱신하여 그립니다. 마우스 이벤트를 별도의 타이머로 처리하지 않고 **디스플레이 링크 콜백 안에서 현재 위치를 읽어** 렌더링하면, 120Hz ProMotion의 프레임 예산에 정확히 맞춰 커서가 이동합니다. 이는 앞서 논의한 `CAMetalLayer`의 `displaySyncEnabled` 및 `maximumDrawableCount = 3` 설정과 자연스럽게 결합됩니다.

#### 4단계: 경계 처리

커서가 캔버스 뷰를 벗어나거나 앱이 비활성화되면, **투명 `NSCursor`를 해제하고 시스템 기본 커서로 복원**해야 합니다. `NSCursor.pop()`을 사용하거나 `NSCursor.arrow.set()`으로 되돌립니다. 또한 커서가 창 밖으로 나가는 순간에는 팬 스프라이트 렌더링을 중단해야 불필요한 드로우 콜을 줄일 수 있습니다.

### 💎 정리: 왜 이 전략이 "확실한"가

| 요소 | 고수준 `NSCursor` 경로 | 투명 커서 + Metal 렌더링 |
| :--- | :--- | :--- |
| 커서 모양 변경 비용 | `resetCursorRects` 수 ms 또는 `set`의 높은 CPU 비용 | 텍스처 바인딩 교체만 발생, 프레임 예산 내 |
| VSync/ProMotion 영향 | `hide()` 시 프레임 드롭 가능 | 하드웨어 커서 평면 유지, Metal 파이프라인과 동일한 타이밍 |
| 애니메이션/상태 전환 | 제한적 | 열린 손/닫힌 손 등 실시간 전환 자유로움 |
| 필기 앱과의 통합 | 별도 경로 | 기존 Metal 렌더 패스에 드로우 콜만 추가 |

이 전략의 핵심은 **"커서를 시스템에 맡기지 않고, 앱의 렌더링 파이프라인 안으로 가져온다"** 는 것입니다. 투명 `NSCursor`로 시스템 커서의 간섭을 제거하고, 팬 모양은 Metal 스프라이트로 그리면, M6/120Hz 환경에서 필기 스트로크와 커서가 동일한 프레임 타이밍으로 움직이는 가장 확실한 고성능 경로를 확보할 수 있습니다.
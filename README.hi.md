<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
<p align="center">
  <img src="https://raw.githubusercontent.com/puneet-chandna/requests-native/main/docs/assets/logo.png" alt="Requests Native" width="250">
</p>

<h1 align="center">Requests Native</h1>

<p align="center">
<a href="https://github.com/puneet-chandna/requests-native/blob/main/README.md">English</a> · <a href="https://github.com/puneet-chandna/requests-native/blob/main/README.es.md">Español</a> · <a href="https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md">简体中文</a> · <a href="https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md">Français</a> · <a href="https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md">हिन्दी</a> · <a href="https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md">日本語</a>
</p>

<p align="center">
Requests Native, Python की परिचित <code>requests</code> API को Rust में लिखे एक साझा HTTP कोर से जोड़ता है।
यही कोर Rust के लिए नेटिव असिंक्रोनस और ब्लॉकिंग क्लाइंट के रूप में भी सीधे उपलब्ध है।
दोनों Rust क्लाइंट कनेक्शन पूलिंग, कॉन्फ़िगर किए जा सकने वाले टाइमआउट, proxy और TLS सेटिंग्स
और अनुरोध व प्रतिक्रिया बॉडी की स्ट्रीमिंग का समर्थन करते हैं।
</p>

<p align="center">
<a href="https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.1">1.0.1 रिलीज़</a> ·
<a href="https://crates.io/crates/requests-native">crates.io</a> ·
<a href="https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst">इंस्टॉलेशन</a> · <a href="https://github.com/puneet-chandna/requests-native/issues">समस्याएँ</a>

</p>

## Rust: स्थिर संस्करण 1.0.1

यह दस्तावेज़ों और संस्करण मेटाडेटा का अपडेट है; कार्यान्वयन 1.0.0 से नहीं बदला है।

**Rust 1.98.1 या उससे नया संस्करण** इस्तेमाल करें और यह जोड़ें:

```toml
[dependencies]
requests-native = "1.0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

क्रेट का नाम `requests-native` है; लाइब्रेरी का इंपोर्ट नाम `requests_native` है।
असिंक्रोनस उदाहरण के लिए Tokio चाहिए। ब्लॉकिंग क्लाइंट डिफ़ॉल्ट रूप से शामिल है।

### असिंक्रोनस

```rust
use requests_native::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send().await?;
    println!("{}", response.text().await?);
    Ok(())
}
```

### ब्लॉकिंग

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

कनेक्शन पूल का दोबारा इस्तेमाल करने के लिए उसी क्लाइंट का दोबारा इस्तेमाल करें।

## Python: 1.0.1 का बेहतर सोर्स कोड इंस्टॉल करें

अगले कुछ दिनों में PyPI पर प्रकाशन की योजना है; प्रकाशन की तैयारी
पूरी की जा रही है। Python वितरण **अभी PyPI पर प्रकाशित नहीं हुआ है**;
फ़िलहाल `v1.0.1` का बेहतर सोर्स कोड इस्तेमाल करें। इस रिलीज़ में Python के स्थिर wheels शामिल नहीं हैं।

आपको **CPython 3.10+**, Git, रिपॉज़िटरी में तय किया गया **Rust 1.98.1** टूलचेन
और एक नेटिव C/C++ कंपाइलर (Windows पर MSVC Build Tools) चाहिए।
[rustup](https://rustup.rs/) से Rust इंस्टॉल करें, फिर एक नया एनवायरनमेंट बनाएँ:

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

Linux/macOS पर इसे सक्रिय करें:

```console
source .venv/bin/activate
```

या Windows PowerShell में:

```powershell
.venv\Scripts\Activate.ps1
```

टैग वाला सोर्स कोड इंस्टॉल करें:

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.1"
```

परिचित API इस्तेमाल करें:

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

वितरण `requests-native` **1.0.1** है; `import requests` और
`requests.__version__ == "2.34.2"`, Requests के साथ संगतता की पहचान बनाए रखते हैं।

**मूल `requests` पैकेज से अलग एनवायरनमेंट इस्तेमाल करें।** दोनों वितरण
एक ही इंपोर्ट पथ का इस्तेमाल करते हैं; `requests` नाम वाले पैकेज पर निर्भरता
मूल Requests को दोबारा इंस्टॉल कर सकती है। [इंस्टॉलेशन का विवरण](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) देखें।

Windows की मौजूदा जाँचें बिना किसी TLS विफलता को स्वीकार किए पास हुई हैं; पुरानी [समस्या #1](https://github.com/puneet-chandna/requests-native/issues/1), जिसे दोबारा उत्पन्न नहीं किया जा सका, अभी खुली है।

## प्रदर्शन

प्रकाशित 1.0.0 Rust कोर को स्थानीय लूपबैक तुलना में पाँच जोड़ी मापों से जाँचा गया, जिसमें हर नेटिव क्लाइंट के लिए 16 समान कार्यभार थे।
संस्करण [1.0.1](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.1) का रनटाइम कार्यान्वयन वही है।

| नेटिव क्लाइंट | मूल Requests के मुकाबले थ्रूपुट का माध्यिका अनुपात | मूल Requests के मुकाबले थ्रूपुट का अधिकतम मापा गया अनुपात | अधिकतम मापा गया थ्रूपुट (अनुरोध/सेकंड) | बीटा के मुकाबले थ्रूपुट का माध्यिका अनुपात |
| --- | --- | --- | --- | --- |
| असिंक्रोनस | 2.65x | 11.92x | 13,479 | 1.005x |
| ब्लॉकिंग | 2.23x | 9.96x | 10,513 | 1.055x |

माध्यिका अनुपात पाँच जोड़ी मापों में हर कार्यभार के लिए निकाले गए 16 माध्यिका अनुपातों की माध्यिका हैं।
अधिकतम मान रिपोर्टों में किसी एक माप के सबसे ऊँचे मान हैं, सामान्य प्रदर्शन नहीं; अधिकतम अनुपात और अधिकतम अनुरोध/सेकंड अलग-अलग मापों से मिल सकते हैं।
लाभ कार्यभार के अनुसार बदलते हैं; ऑरेकल नियंत्रणों की अस्थिरता के कारण सांख्यिकीय अनिश्चितता बनी हुई है।
[पद्धति](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md) और [सटीक सोर्स कोड से जुड़े प्रमाण](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md) देखें।

## संगतता और विकास

अंतर्निहित `Session`/`HTTPAdapter` का ट्रैफ़िक नेटिव ट्रांसपोर्ट इस्तेमाल करता है। कस्टम
एडैप्टर, सबक्लास, मंकीपैचिंग और असमर्थित एक्सटेंशन व्यवहार के लिए नेटिव I/O से पहले
Python पर वापस जाया जा सकता है। अपने कार्यभार के साथ इंटीग्रेशन की जाँच करें।

[API संगतता सूची](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv),
[पोर्टिंग गाइड](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md) और
[योगदान गाइड](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md) देखें।
सुरक्षा कमज़ोरियों की सूचना [सुरक्षा नीति](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md) के अनुसार दें।

## श्रेय

Requests Native, [PSF Requests](https://github.com/psf/requests) का
**गैर-आधिकारिक, स्वतंत्र** पुनर्लेखन है, जिसका रखरखाव
[Puneet Chandna](https://github.com/puneet-chandna) करते हैं। इसकी Python Software Foundation
या मूल Requests के रखरखावकर्ताओं से कोई संबद्धता नहीं है और न ही उनका समर्थन प्राप्त है।

यह व्युत्पन्न परियोजना Apache-2.0 लाइसेंस, सूचनाएँ, इतिहास और मूल
योगदानकर्ताओं का रिकॉर्ड बनाए रखती है। [LICENSE](LICENSE), [NOTICE](NOTICE),
[AUTHORS.rst](AUTHORS.rst), [रनटाइम सूचनाएँ](RUST_RUNTIME_NOTICES.html) और
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md) देखें।

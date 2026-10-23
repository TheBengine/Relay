#include "api/crypto/frame_crypto_transformer.h"
#include "api/frame_transformer_interface.h"
#include "api/make_ref_counted.h"
#include "modules/rtp_rtcp/source/rtp_video_header.h"
#include "rtc_base/thread.h"
#include "rtc_base/logging.h"
#include <condition_variable>
#include <mutex>
#include <chrono>
#include <cstdio>
#include <vector>
#include <set>

namespace webrtc {
// The SDK explicitly grants this mock class access to its frame Passkey.
class MockTransformableVideoFrame : public TransformableVideoFrameInterface {
 public:
  explicit MockTransformableVideoFrame(Direction direction = Direction::kSender) : TransformableVideoFrameInterface(Passkey{}), direction_(direction) {
    data_.assign(32, 0x55); header_.codec = VideoCodecType::kVideoCodecVP8;
  }
  std::span<const uint8_t> GetData() const override { return data_; }
  void SetData(std::span<const uint8_t> data) override { data_.assign(data.begin(),data.end()); }
  uint8_t GetPayloadType() const override { return 96; }
  uint32_t GetSsrc() const override { return 42; }
  uint32_t GetTimestamp() const override { return 1000; }
  void SetRTPTimestamp(uint32_t) override {}
  Direction GetDirection() const override { return direction_; }
  std::string GetMimeType() const override { return "video/VP8"; }
  std::optional<Timestamp> ReceiveTime() const override { return std::nullopt; }
  std::optional<Timestamp> CaptureTime() const override { return std::nullopt; }
  std::optional<TimeDelta> SenderCaptureTimeOffset() const override { return std::nullopt; }
  bool IsKeyFrame() const override { return false; }
  VideoFrameMetadata Metadata() const override { return {}; }
  void SetMetadata(const VideoFrameMetadata&) override {}
  const RTPVideoHeader& header() const override { return header_; }
 private:
  std::vector<uint8_t> data_;
  RTPVideoHeader header_;
  Direction direction_;
};
}

class OwnedCallback : public webrtc::TransformedFrameCallback {
 public:
  void OnTransformedFrame(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    std::lock_guard<std::mutex> lock(mutex_);
    auto data=frame->GetData(); data_.assign(data.begin(),data.end()); ready_=true; changed_.notify_one();
  }
  std::vector<uint8_t> Wait(int timeout_ms = 3000) {
    std::unique_lock<std::mutex> lock(mutex_);
    if(!changed_.wait_for(lock,std::chrono::milliseconds(timeout_ms),[&]{return ready_;})) return {};
    ready_=false; return data_;
  }
 private:
  std::mutex mutex_; std::condition_variable changed_; bool ready_=false; std::vector<uint8_t> data_;
};


class OwnedObserver : public webrtc::FrameCryptorTransformerObserver {
 public:
  void OnFrameCryptionStateChanged(const std::string, webrtc::FrameCryptionState state) override {
    std::lock_guard<std::mutex> lock(mutex_); states_.insert(state); changed_.notify_one();
  }
  bool Wait(webrtc::FrameCryptionState state) {
    std::unique_lock<std::mutex> lock(mutex_);
    return changed_.wait_for(lock,std::chrono::seconds(3),[&]{return states_.contains(state);});
  }
 private:
  std::mutex mutex_; std::condition_variable changed_; std::set<webrtc::FrameCryptionState> states_;
};

extern "C" int relay_owned_iv_probe() {
  webrtc::LogMessage::LogToDebug(webrtc::LS_NONE);
  webrtc::LogMessage::SetLogToStderr(false);
  auto signaling=webrtc::Thread::Create(); if(!signaling->Start()) return 2;
  webrtc::KeyProviderOptions options; options.key_derivation_algorithm=webrtc::KeyDerivationAlgorithm::kHKDF; options.failure_tolerance=0;
  auto provider=webrtc::make_ref_counted<webrtc::DefaultKeyProviderImpl>(options);
  provider->SetKey("owned-synthetic-participant",0,std::vector<uint8_t>(32,23));
  auto make_cryptor=[&] {
    webrtc::scoped_refptr<webrtc::FrameCryptorTransformer> cryptor(new webrtc::FrameCryptorTransformer(
      signaling.get(),"owned-synthetic-participant",webrtc::FrameCryptorTransformer::MediaType::kVideoFrame,
      webrtc::FrameCryptorTransformer::Algorithm::kAesGcm,provider));
    cryptor->SetEnabled(true); return cryptor;
  };
#if defined(RELAY_PROBE_RNG_FAILURE) || defined(RELAY_PROBE_QUOTA)
  auto sender=make_cryptor(); auto callback=webrtc::make_ref_counted<OwnedCallback>();
  auto observer=webrtc::make_ref_counted<OwnedObserver>();
  sender->RegisterFrameCryptorTransformerObserver(observer);
  webrtc::FrameTransformerInterface* transform=sender.get();
  transform->RegisterTransformedFrameSinkCallback(callback,42);
  int accepted=0;
#ifdef RELAY_PROBE_QUOTA
  for(int i=0;i<2;++i) {
    transform->Transform(std::make_unique<webrtc::MockTransformableVideoFrame>());
    if(callback->Wait().size()!=62) return 10; ++accepted;
  }
#endif
  transform->Transform(std::make_unique<webrtc::MockTransformableVideoFrame>());
  if(!observer->Wait(webrtc::FrameCryptionState::kEncryptionFailed) || !callback->Wait(300).empty()) return 11;
  // A new cryptor does not reset the shared invocation budget, and repeated
  // failure never falls through to plaintext or encrypted output.
  auto next=make_cryptor(); auto next_callback=webrtc::make_ref_counted<OwnedCallback>();
  auto next_observer=webrtc::make_ref_counted<OwnedObserver>();
  next->RegisterFrameCryptorTransformerObserver(next_observer);
  webrtc::FrameTransformerInterface* next_transform=next.get();
  next_transform->RegisterTransformedFrameSinkCallback(next_callback,42);
  next_transform->Transform(std::make_unique<webrtc::MockTransformableVideoFrame>());
  if(!next_observer->Wait(webrtc::FrameCryptionState::kEncryptionFailed) || !next_callback->Wait(300).empty()) return 12;
  next_transform->UnregisterTransformedFrameSinkCallback(42); next->UnRegisterFrameCryptorTransformerObserver();
  transform->UnregisterTransformedFrameSinkCallback(42); sender->UnRegisterFrameCryptorTransformerObserver();
  std::printf("{\"fault_injection\":true,\"accepted_before_limit\":%d,\"no_output_on_failure\":true,\"new_cryptor_still_refused\":true}\n",accepted);
#else
  std::set<std::vector<uint8_t>> ivs;
  int encrypted=0, decrypted=0, rejected=0;
  for(int instance=0;instance<32;++instance) {
    provider->SetKey("owned-synthetic-participant",0,std::vector<uint8_t>(32,23));
    auto sender=make_cryptor(), receiver=make_cryptor();
    auto send_callback=webrtc::make_ref_counted<OwnedCallback>(), receive_callback=webrtc::make_ref_counted<OwnedCallback>();
    auto observer=webrtc::make_ref_counted<OwnedObserver>(); receiver->RegisterFrameCryptorTransformerObserver(observer);
    webrtc::FrameTransformerInterface* send_transform=sender.get();
    webrtc::FrameTransformerInterface* receive_transform=receiver.get();
    send_transform->RegisterTransformedFrameSinkCallback(send_callback,42);
    receive_transform->RegisterTransformedFrameSinkCallback(receive_callback,42);
    std::vector<uint8_t> wire;
    for(int frame=0;frame<8;++frame) {
      send_transform->Transform(std::make_unique<webrtc::MockTransformableVideoFrame>());
      wire=send_callback->Wait();
      if(wire.size()!=62 || wire[wire.size()-2]!=12) return 3;
      if(!ivs.insert(std::vector<uint8_t>(wire.end()-14,wire.end()-2)).second) return 4;
      ++encrypted;
      auto incoming=std::make_unique<webrtc::MockTransformableVideoFrame>(webrtc::TransformableFrameInterface::Direction::kReceiver);
      incoming->SetData(wire); receive_transform->Transform(std::move(incoming));
      if(receive_callback->Wait()!=std::vector<uint8_t>(32,0x55)) return 5;
      ++decrypted;
    }
    // Alter the AEAD tag, preserving codec prefix and IV/trailer structure.
    wire[wire.size()-15]^=1;
    auto tampered=std::make_unique<webrtc::MockTransformableVideoFrame>(webrtc::TransformableFrameInterface::Direction::kReceiver);
    tampered->SetData(wire); receive_transform->Transform(std::move(tampered));
    if(!observer->Wait(webrtc::FrameCryptionState::kDecryptionFailed) || !receive_callback->Wait(100).empty()) return 6;
    ++rejected;
    send_transform->UnregisterTransformedFrameSinkCallback(42);
    receive_transform->UnregisterTransformedFrameSinkCallback(42); receiver->UnRegisterFrameCryptorTransformerObserver();
  }
  std::printf("{\"sdk\":\"webrtc-89d790b\",\"synthetic_video_frames\":%d,\"independent_sender_cryptors\":32,\"same_key_ssrc_timestamp\":true,\"iv_bytes\":12,\"sample_iv_collisions\":0,\"native_decrypted\":%d,\"tampered_tags_rejected\":%d,\"physical_media\":false}\n",encrypted,decrypted,rejected);
#endif
  std::fflush(stdout); signaling->Stop(); return 0;
}

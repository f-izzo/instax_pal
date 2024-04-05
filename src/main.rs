use bluest::{pairing::NoInputOutputPairingAgent, Adapter, Uuid, Characteristic};
use futures_lite::{StreamExt, Stream};
use instax_pal::*;
use std::{error::Error, pin::Pin};
use num_traits::FromPrimitive;
use std::{thread, time::Duration};
use chrono::prelude::*;

// UART-like GATT service
// Commands are sent to INSTAX_WRITE_UUID characteristic
// Responses are read from INSTAX_NOTIFY_UUID characteristic
// Reference: https://github.com/jpwsutton/instax_api/issues/21#issuecomment-770462168
const INSTAX_SERVICE_UUID: Uuid = Uuid::from_u128(0x70954782_2d83_473d_9e5f_81e1d02d5273);
const INSTAX_WRITE_UUID: Uuid = Uuid::from_u128(0x70954783_2d83_473d_9e5f_81e1d02d5273);
const INSTAX_NOTIFY_UUID: Uuid = Uuid::from_u128(0x70954784_2d83_473d_9e5f_81e1d02d5273);

#[derive(Debug)]
enum PacketType {
    Sid = 0,
    Type = 1,
    Data = 3,
}

#[derive(Debug)]
struct Packet {
    p_type: PacketType,
    direction: Direction,
    size: u16,
    sid: SID,
    msg_type: u8,
    data: Vec<u8>
}
impl Packet {
    fn pack(&self) -> Vec<u8> {
        let mut packet: Vec<u8> = Vec::new();
        packet.extend((self.direction as u16).to_be_bytes());
        packet.extend(self.size.to_be_bytes());
        packet.extend((self.sid as u16).to_be_bytes());
        if matches!(self.p_type, PacketType::Type) {
            packet.push(self.msg_type);
        }
        if matches!(self.p_type, PacketType::Data) {
            packet.extend(&self.data);
        }
        // Add checksum
        let checksum: u8 = 255 - packet.iter().fold(0, |a: u8, &b| a.wrapping_add(b));
        packet.push(checksum);
        packet
    }
    fn unpack(msg: &Vec<u8>) -> Self {
        let p_type = match msg.len() {
            0..=7 => panic!("ERROR: Packet too short. len:{}", msg.len()),
            8 => PacketType::Sid,
            9 => PacketType::Type,
            _ => PacketType::Data,
        };
        match p_type {
            PacketType::Sid => {
                let direction = FromPrimitive::from_u16(u16::from_be_bytes(msg[0..2].try_into().unwrap())).unwrap();
                let size = u16::from_be_bytes(msg[2..4].try_into().unwrap());
                let sid = FromPrimitive::from_u16(u16::from_be_bytes(msg[4..6].try_into().unwrap())).unwrap();
                let msg_type = msg[6];
                let data = Vec::new();
                Packet{p_type, direction, size, sid, msg_type, data}
            }
            PacketType::Type => {
                let direction = FromPrimitive::from_u16(u16::from_be_bytes(msg[0..2].try_into().unwrap())).unwrap();
                let size = u16::from_be_bytes(msg[2..4].try_into().unwrap());
                let sid = FromPrimitive::from_u16(u16::from_be_bytes(msg[4..6].try_into().unwrap())).unwrap();
                let msg_type = msg[6];
                let data: Vec<u8> = msg[7..((msg.len() - 1) as usize)].to_vec();
                Packet{p_type, direction, size, sid, msg_type, data}
            }
            PacketType::Data => {
                let direction = FromPrimitive::from_u16(u16::from_be_bytes(msg[0..2].try_into().unwrap())).unwrap();
                let size = u16::from_be_bytes(msg[2..4].try_into().unwrap());
                let sid = FromPrimitive::from_u16(u16::from_be_bytes(msg[4..6].try_into().unwrap())).unwrap();
                let msg_type = 0;
                let data: Vec<u8> = msg[6..((msg.len() - 1) as usize)].to_vec();
                Packet{p_type, direction, size, sid, msg_type, data}
            }
        }
    }
    fn with_sid(sid: SID) -> Self {
        Packet {
            p_type: PacketType::Sid,
            direction: Direction::TO,
            size: 7, // Direction(2) + Size (2) + SID (2) + Checksum (1)
            sid,
            msg_type: 0,
            data: vec![],
        }
    }
    fn with_type(sid: SID, msg_type: u8) -> Self {
        Packet {
            p_type: PacketType::Type,
            direction: Direction::TO,
            size: 8, // Direction(2) + Size (2) + SID (2) + Type (1) + Checksum (1)
            sid,
            msg_type,
            data: vec![],
        }
    }
    fn with_data(sid: SID, data: Vec<u8>) -> Self {
        Packet {
            p_type: PacketType::Data,
            direction: Direction::TO,
            size: 7 + data.len() as u16, // Direction(2) + Size (2) + SID (2) + Payload (N) + Checksum (1)
            sid,
            msg_type: 0,
            data: data,
        }
    }
}

struct Camera {
    write_char: Characteristic,
    notify_stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, bluest::Error>> + Send + Unpin>>,
}
impl Camera {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let adapter = Adapter::default()
            .await
            .ok_or("Bluetooth adapter not found")?;
        adapter.wait_available().await?;
        println!("Searching Instax device");
        let device = adapter
            .discover_devices(&[INSTAX_SERVICE_UUID])
            .await?
            .next()
            .await
            .ok_or("Failed to discover device")??;
        println!("found device: {} ({:?})",
            device.name().as_deref().unwrap_or("(unknown)"),
            device.id()
        );
        // HACK: on some laptops we need to repeat pairing, otherwise we can't connect
        if device.is_paired().await? {
            println!("Repeating device pairing");
            device.unpair().await?
        }
        device.pair_with_agent(&NoInputOutputPairingAgent).await?;
        adapter.connect_device(&device).await?;
        println!("connected!");
        let service = match device
            .discover_services_with_uuid(INSTAX_SERVICE_UUID)
            .await?
            .get(0)
        {
            Some(service) => service.clone(),
            None => return Err("Service not found".into()),
        };
        let characteristics = service.characteristics().await?;
        let write_char = characteristics
            .iter()
            .find(|x| x.uuid() == INSTAX_WRITE_UUID)
            .ok_or("write characteristic not found")?
            .clone();
        let notify_char = characteristics
            .iter()
            .find(|x| x.uuid()  == INSTAX_NOTIFY_UUID)
            .ok_or("notify characteristic not found")?
            .clone();
        let notify_stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, bluest::Error>> + Send + Unpin>> = Box::pin(notify_char.notify().await?);
        let camera = Camera{write_char, notify_stream};
        Ok(camera)
    }

    async fn send_packet(&mut self, packet: Packet) -> Result<(), Box<dyn Error>> {
        let data = packet.pack();
        println!("SENT: {:x?}", &data);
        self.write_char.write(&data).await?;
        Ok(())
    }
    
    async fn receive_packet(&mut self) -> Option<Packet> {
        while let Some(msg) = self.notify_stream.next().await {
            let data = &msg.unwrap();
            println!("RECV: {:x?}", &data);
            let packet = Packet::unpack(&data);
            return Some(packet);
        }
        None
    }
    
    async fn receive_data(&mut self) -> Option<Vec<u8>> {
        let mut data: Vec<u8> = Vec::new();
        while let Some(msg) = self.notify_stream.next().await {
            let payload = msg.unwrap();
            data.extend(&payload);
            println!("RECV: {:x?}", &payload);
            if payload.len() == 1 {
                return Some(data)
            }
        }
        None
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut camera = Camera::new().await?;
    support_function_version_info(&mut camera).await;
    parameter_read(&mut camera, ReadWriteSettingType::TRANSFER_FORMAT).await;
    parameter_read(&mut camera, ReadWriteSettingType::FLASH_SETTING).await;
    set_timedate(&mut camera).await;
    support_function_info(&mut camera, SupportFunctionInfoType::IMAGE_SUPPORT_INFO).await;
    support_function_info(&mut camera, SupportFunctionInfoType::BATTERY_INFO).await;
    support_function_info(&mut camera, SupportFunctionInfoType::CAMERA_FUNCTION_INFO).await;
    support_function_info(&mut camera, SupportFunctionInfoType::CAMERA_HISTORY_INFO).await;
    //automatic_photo_download(&mut camera).await;
    live_view_test(&mut camera).await;
    Ok(())
}

async fn support_function_version_info(camera: &mut Camera) {
    let packet = Packet::with_sid(SID::SUPPORT_FUNCTION_AND_VERSION_INFO);
    camera.send_packet(packet).await.unwrap();
    let response = camera.receive_packet().await.unwrap();
    let info = SupportFunctionVersionInfo::from_bytes(&response.data);
    dbg!(info);
}

async fn support_function_info(camera: &mut Camera, info_type: SupportFunctionInfoType) {
    let packet = Packet::with_type(SID::SUPPORT_FUNCTION_INFO, info_type.clone() as u8);
    camera.send_packet(packet).await.unwrap();
    let response = camera.receive_packet().await.unwrap();
    match &info_type {
        SupportFunctionInfoType::IMAGE_SUPPORT_INFO => {
            let info = ImageSupportInfo::from_bytes(&response.data);
            dbg!(info);
        }
        SupportFunctionInfoType::BATTERY_INFO => {
            let info = BatteryInfo::from_bytes(&response.data);
            dbg!(&info);
        }
        SupportFunctionInfoType::CAMERA_FUNCTION_INFO => {
            let info = CameraFunctionInfo::from_bytes(&response.data);
            dbg!(&info);
        }
        SupportFunctionInfoType::CAMERA_HISTORY_INFO => {
            let info = CameraHistoryInfo::from_bytes(&response.data);
            dbg!(&info);
        }
        _ => panic!("Info type: {:?} not implemented", &info_type)
    }
}

async fn parameter_read(camera: &mut Camera, setting: ReadWriteSettingType) {
    let payload = vec![setting as u8, ReadWriteSettingMode::GET_CURRENT_SETTING as u8, 0x00, 0x00, 0x00, 0x00];
    let packet = Packet::with_data(SID::PARAMETER_RW, payload);
    camera.send_packet(packet).await.unwrap();
    let response = camera.receive_packet().await.unwrap();
    let info = ParameterReadWriteResponse::from_bytes(&response.data);
    dbg!(info);
}

async fn set_timedate(camera: &mut Camera) {
    let now = Utc::now();
    let formatted = now.format("%Y%m%d%H%M%S").to_string();
    let mut bytes = formatted.into_bytes();
    let mut payload: Vec<u8> = vec![2];
    payload.append(&mut bytes);
    let packet = Packet::with_data(SID::TIME_SETTING, payload);
    camera.send_packet(packet).await.unwrap();
    let response = camera.receive_packet().await.unwrap();
    let info = DateTimeResponse::from_bytes(&response.data);
    dbg!(info);
}

async fn automatic_photo_download(camera: &mut Camera) {
    println!("Auto upload info");
    let packet = Packet::with_sid(SID::IMAGE_AUTO_UPLOAD_INFO);
    camera.send_packet(packet).await.unwrap();
    let response = camera.receive_packet().await.unwrap();
    if response.data[0] == 0x81 {
        println!("No photo available");
        return;
    }
    println!("Auto upload start");
    let packet = Packet::with_data(SID::IMAGE_AUTO_UPLOAD_START, vec![0;4]);
    camera.send_packet(packet);
    let response = camera.receive_packet().await.unwrap();
    println!("Auto upload data");
    let num_frames = response.data[3];
    println!("Receiving {} frames", num_frames);
    for frame in 0..50 {
        let frame_num = (frame as u32).to_be_bytes().to_vec();
        let packet = Packet::with_data(SID::IMAGE_AUTO_UPLOAD_DATA, frame_num);
        camera.send_packet(packet);
        let _data = camera.receive_data().await.unwrap();
        println!("Frame: {}", frame);
        thread::sleep(Duration::from_millis(600));
    }
}

async fn live_view_test(camera: &mut Camera) {
    println!("Live view start");
    let packet = Packet::with_type(SID::LIVE_VIEW_START, 0);
    camera.send_packet(packet).await.unwrap();
    let _response = camera.receive_packet().await.unwrap();
    println!("Live view receive");
    let packet = Packet::with_sid(SID::LIVE_VIEW_RECEIVE);
    camera.send_packet(packet);
    thread::sleep(Duration::from_millis(600));
    let _response = camera.receive_packet().await.unwrap();
    let packet = Packet::with_sid(SID::LIVE_VIEW_RECEIVE);
    camera.send_packet(packet);
    let _data = camera.receive_data().await.unwrap();
}

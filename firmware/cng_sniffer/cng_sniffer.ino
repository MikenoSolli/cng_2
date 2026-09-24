#include <ESP8266WiFi.h>
#include <SoftwareSerial.h>


#define RE 14

SoftwareSerial mod(0, 2);
WiFiClient client;


const char *WIFI_SSID   = "your-ssid";
const char *WIFI_PASS   = "your-password";
const char *SERVER_IP   = "192.46.236.241";
const uint16_t SERVER_PORT = 5000;
const char *DEVICE_ID   = "D01";

const float PRICE = 1550;
const unsigned long SILENCE_TIMEOUT = 10000;   // close a fill after 10 s of silence
const unsigned long IDLE_STATUS_MS  = 5000;    // status interval when idle
const unsigned long FILL_STATUS_MS  = 2000;    // status interval while filling
const unsigned long RECONNECT_MS    = 5000;
const unsigned long RESEND_MS       = 3000;    // resend an un-ACKed fill report


byte values[64];
byte count = 0;
unsigned long lastByte = 0;
unsigned long lastFrame = 0;
unsigned long badCrc = 0;

float served_now = 0;
float total_served = 0;
float flow = 0;
float temp = 0;
bool filling = false;

unsigned long lastStatus = 0;
unsigned long lastConnectTry = 0;
unsigned long lastSend = 0;

unsigned long bootId;
unsigned long fillCounter = 0;

// Fill reports waiting for ACK
#define QUEUE_SIZE 20
unsigned long queueNum[QUEUE_SIZE];
float queueKg[QUEUE_SIZE];
float queueTotal[QUEUE_SIZE];
const char *queueReason[QUEUE_SIZE];
unsigned long queueTime[QUEUE_SIZE];
byte queueCount = 0;



uint16_t crc16(byte *buf, byte len) {
  uint16_t crc = 0xFFFF;
  for (byte i = 0; i < len; i++) {
    crc ^= buf[i];
    for (byte b = 0; b < 8; b++) {
      if (crc & 1) crc = (crc >> 1) ^ 0xA001;
      else crc = crc >> 1;
    }
  }
  return crc;
}


bool crcOk(byte len) {
  if (len < 4) return false;
  uint16_t received = values[len - 2] | (values[len - 1] << 8);
  return crc16(values, len - 2) == received;
}


float getFloat(byte i) {   // word-swapped (CDAB)
  uint32_t raw = ((uint32_t)values[i + 2] << 24) | ((uint32_t)values[i + 3] << 16) |
                 ((uint32_t)values[i] << 8) | values[i + 1];
  float f;
  memcpy(&f, &raw, 4);
  return f;
}


void serveReport(const char *reason) {
  filling = false;
  total_served = total_served + served_now;
  fillCounter++;

  Serial.println();
  Serial.print("fill closed by:: ");
  Serial.println(reason);
  Serial.print("served_now:: ");
  Serial.println(served_now, 3);
  Serial.print("amount:: ");
  Serial.println(served_now * PRICE, 2);
  Serial.print("total served:: ");
  Serial.println(total_served, 3);
  Serial.println();

  if (queueCount == QUEUE_SIZE) {
    Serial.println("QUEUE FULL - oldest report dropped");
    for (byte i = 1; i < QUEUE_SIZE; i++) {
      queueNum[i - 1] = queueNum[i];
      queueKg[i - 1] = queueKg[i];
      queueTotal[i - 1] = queueTotal[i];
      queueReason[i - 1] = queueReason[i];
      queueTime[i - 1] = queueTime[i];
    }
    queueCount--;
  }

  queueNum[queueCount] = fillCounter;
  queueKg[queueCount] = served_now;
  queueTotal[queueCount] = total_served;
  queueReason[queueCount] = reason;
  queueTime[queueCount] = millis();
  queueCount++;

  lastSend = 0;       // send right away
  lastStatus = 0;     // and report IDLE right away
}


void sendStatus() {
  char line[96];
  if (filling) {
    snprintf(line, sizeof(line), "S,%s,FILLING,%.1f,%.1f,%.3f\n",
             DEVICE_ID, flow, temp, served_now);
  } else {
    snprintf(line, sizeof(line), "S,%s,IDLE,%lu,%lu\n",
             DEVICE_ID, millis(), badCrc);
  }
  client.print(line);
}


void sendOldestFill() {
  char line[128];
  snprintf(line, sizeof(line), "F,%s,%lX-%lu,%.3f,%.2f,%.3f,%s,%lu\n",
           DEVICE_ID, bootId, queueNum[0], queueKg[0], queueKg[0] * PRICE,
           queueTotal[0], queueReason[0], millis() - queueTime[0]);
  client.print(line);
  Serial.print("TX ");
  Serial.print(line);
}


void checkAck() {
  while (client.available()) {
    String reply = client.readStringUntil('\n');
    reply.trim();

    if (queueCount > 0) {
      char expected[40];
      snprintf(expected, sizeof(expected), "ACK,%lX-%lu", bootId, queueNum[0]);

      if (reply == expected) {
        Serial.println(reply);
        for (byte i = 1; i < queueCount; i++) {
          queueNum[i - 1] = queueNum[i];
          queueKg[i - 1] = queueKg[i];
          queueTotal[i - 1] = queueTotal[i];
          queueReason[i - 1] = queueReason[i];
          queueTime[i - 1] = queueTime[i];
        }
        queueCount--;
        lastSend = 0;   // next one straight away
      }
    }
  }
}



void setup(void) {
  Serial.begin(115200);
  mod.begin(9600);
  pinMode(RE, OUTPUT);
  digitalWrite(RE, LOW);   // listen only

  bootId = ESP.random();

  WiFi.mode(WIFI_STA);
  WiFi.begin(WIFI_SSID, WIFI_PASS);   // connects in the background
  client.setTimeout(1000);
  client.setNoDelay(true);

  Serial.println(" ");
  Serial.println("Sniffing Code + TCP");
  Serial.println(" ");

}


void loop(void) {

  // ---------- Sniffing ----------
  while (mod.available()) {
    if (count < 64) {
      values[count] = mod.read();
      count++;
    } else {
      mod.read();
    }
    lastByte = millis();
  }

  if (count > 0 && millis() - lastByte > 5) {

    if (!crcOk(count)) {
      badCrc++;
      count = 0;
      return;
    }

    lastFrame = millis();

    // Meter reply during a fill: 01 03 1C ... (33 bytes)
    if (count == 33 && values[0] == 0x01 && values[1] == 0x03 && values[2] == 0x1C) {
      if (!filling) {
        filling = true;
        lastStatus = 0;   // report FILLING right away
        Serial.println("fill started");
      }
      flow = getFloat(3);          // 40247 kg/h
      temp = getFloat(11);         // 40251 C
      served_now = getFloat(27);   // 40259 kg this fill
    }

    // Idle write 01 06 00 07 ... means the fill has ended
    if (count == 8 && values[0] == 0x01 && values[1] == 0x06 && filling) {
      serveReport("idle");
    }

    count = 0;
  }

  if (filling && millis() - lastFrame > SILENCE_TIMEOUT) {
    serveReport("silence");
  }


  // ---------- Network (never in the middle of a frame) ----------
  if (count > 0) return;

  // Connecting can block ~1 s, so only while idle
  if (!filling && !client.connected() && WiFi.status() == WL_CONNECTED &&
      millis() - lastConnectTry > RECONNECT_MS) {
    lastConnectTry = millis();
    if (client.connect(SERVER_IP, SERVER_PORT)) {
      Serial.println("TCP connected");
      lastStatus = 0;
      lastSend = 0;
    } else {
      Serial.println("TCP connect failed");
    }
  }

  if (!client.connected()) return;

  checkAck();

  unsigned long interval = filling ? FILL_STATUS_MS : IDLE_STATUS_MS;
  if (lastStatus == 0 || millis() - lastStatus > interval) {
    lastStatus = millis();
    sendStatus();
  }

  if (!filling && queueCount > 0 && (lastSend == 0 || millis() - lastSend > RESEND_MS)) {
    lastSend = millis();
    sendOldestFill();
  }

}
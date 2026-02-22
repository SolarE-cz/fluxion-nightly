2 Protocol Overview 2.1 Protocol Type  MODBUS RTU (For RS485)  Address: 1 (default)  Baud rate:
19200 (default)  Data bits: 8  Stop Bit: 1  Parity: None  Frame format: The MODBUS protocol
defines a simple protocol data unit (PDU) independent of the underlying communication layers. The
mapping of MODBUS protocol on specific buses or network can introduce some additional fields on the
application data unit (ADU). Note: The slave address of the MODBUS RTU in the SolaX Power energy
storage system represents the address of the inverter. The address range is assigned as follows:
Broadcast Address Slave Address Reserved 0 1–247 248–255 The size of the MODBUS PDU is limited by
the size constraint inherited from the first MODBUS implementation on Serial Line network (Max.
RS485 ADU = 256 bytes). Therefore: MODBUS PDU for serial line communication = 256 - Server address
(1 byte) - CRC (2 bytes) = 253 bytes. Consequently: RS485 ADU = 253 bytes + Server address (1 byte)
\+ CRC (2 bytes) = 256 bytes. 2  Modbus TCP (For Monitoring Module)  Port: 502  Transaction ID: No
compulsory requirements  Protocol ID: No compulsory requirements  Unit ID: No compulsory
requirements, use 0x01 by default  Frame format: Note: The inverter itself does not support Modbus
TCP functionality. Function expansion must be completed through SolaX's monitoring module. Since it
is used for external expansion, the query cycle should be controlled within approximately 1 second.
2.2 Reading and Writing of Data 2.2.1 MODBUS Command Type Type Hexadecimal Value Data Volume (Number
of Registers)\* Read Holding Registers 0x03 1 to 125 Read Input Registers 0x04 1 to 125 Write Single
Registers 0x06 1 Write Multiple Registers 0x10 1 to 123 \*: Number of MODBUS registers transferable
as a data block per command (16 bit) 2.2.2 Register Address, Width and Data Block A MODBUS register
is 16 bits wide. For wider data items, connected MODBUS registers are used and considered as data
blocks. The address of the first MODBUS register in a data block is the start address of the data
block. 2.2.3 Data Transmission With data storage in the Motorola format "Big Endian", data
transmission begins with the high byte and then the low byte of the MODBUS register. 3 2.2.4 Time
Request Timing Parameter The least interval time between two instructions Character-gap time out
(silent time between 2 packages) Response timeout Value 1 sec

> 100 ms 1 sec 4 2.2.5 Data Format Description  Data FormatField Name Slave ID Function Code Start
> Register Address Register Number CRC Slave ID Function Code Byte Number Register Data CRC Slave ID
> Fault Code Abnormal Code CRC (Read Holding Register) Master Request Format Number of Content
> Format Bytes 0x00~0xFF 1 byte (Inverter default 0x01) 1 byte 0x03 2 bytes Address MSB
> 0x0000-0xFFFF Address LSB 2 bytes Data MSB N Data LSB 2 bytes CRC MSB 0x0000-0xFFFF CRC LSB Slave
> Normal Response 0x00~0xFF 1 byte (Inverter default 0x01) 1 byte 0x03 1 byte Data N*2 bytes Data
> MSB Data LSB 2 bytes CRC MSB CRC LSB 11byte byte 2*N 0x0000-0xFFFF 0x0000-0xFFFF Slave Fault
> Response 0x00~0xFF (Inverter default 0x01) 0x83 1 byte 2 bytes CRC MSB CRC LSB 0x01 or 0x02 or
> 0x03 or 0x04 0x0000-0xFFFF 5 Example 0x01 0x03 0x03 0x24 0x00 0x01 0xC4 0x45 0x01 0x03 0x02 0x00
> 0x00 0xB8 0x44 0x01 0x83 0x02 0xC5 0x3B  Data Format (Read Input Register) Master Request Format
> Field Name Number of Bytes Content Format 0x00~0xFF Slave ID 1 byte (Inverter default 0x01)
> Function Code 1 byte 0x04 2 bytes Start Register Address MSB 0x0000-0xFFFF Address Address LSB 2
> bytes Register Data MSB N Number Data LSB 2 bytes CRC CRC MSB 0x0000-0xFFFF CRC LSB Slave Normal
> Response 0x00~0xFF Slave ID 1 byte (Inverter default 0x01) Function Code 1 byte 0x04 1 byte Byte
> Number 2*N Data N*2 bytes Register Date Data MSB 0x0000-0xFFFF Data LSB 2 bytes CRC CRC MSB
> 0x0000-0xFFFF CRC LSB Slave Fault Response 0x00~0xFF Slave ID 1 byte (Inverter default 0x01) Fault
> Code 1 byte 0x84 Abnormal Code 1 byte 0x01 or 0x02 or 0x03 or 0x04 CRC 2 bytes CRC MSB CRC LSB
> 0x0000-0xFFFF Example 0x01 0x04 0x04 0x15 0x00 0x01 0x21 0x3E 0x01 0x04 0x02 0x00 0x00 0xB9 0x30
> 0x01 0x84 0x02 0xC2 6  Data Format (Write Single Register) Master Request Format Field Name
> Number of Bytes Content Format 0x00~0xFF Slave ID 1 byte (Inverter default 0x01) Function Code 1
> byte 0x06 2 bytes Register Address Address MSB 0x0000-0xFFFF Address LSB 2 bytes Value Data MSB
> 0x0000-0xFFFF Data LSB 2 bytes CRC CRC MSB 0x0000-0xFFFF CRC LSB Slave Normal Response 0x00~0xFF
> Slave ID 1 byte (Inverter default 0x01) Function Code 1 byte 0x06 2 bytes Register Address Address
> MSB 0x0000-0xFFFF Address LSB 2 bytes Value Data MSB 0x0000-0xFFFF Data LSB 2 bytes CRC CRC MSB
> 0x0000-0xFFFF CRC LSB Slave Fault Response 0x00~0xFF Slave ID 1 byte (Inverter default 0x01) Fault
> Code 1 byte 0x86 AbnormalCRC Code 1 byte 2 bytes CRC MSB CRC LSB 0x01 or 0x02 or 0x03 or 0x04
> 0x0000-0xFFFF Example 0x01 0x06 0x06 0x16 0x00 0x01 0xA9 0x46 0x01 0x06 0x06 0x16 0x00 0x01 0xA9
> 0x46 0x01 0x86

-

0xC3 0xA1 7  Data Format (Write Multiple Register) Master Request Format Field Name Number of Bytes
Content Format 0x00~0xFF Slave ID 1 byte (Inverter default 0x01) Function Code 1 byte 0x10 2 bytes
Register Address MSB 0x0000-0xFFFF Address Address LSB 2 bytes Register Number MSB 0x0001-0x007B
Number Number LSB Byte Number 1 byte 2*N Value 2*N bytes Data MSB Data LSB 0x0000-0xFFFF CRC Slave
ID FunctionRegister Address Code Register Number 2 bytes CRC MSB 0x0000-0xFFFF CRC LSB Slave Normal
Response 0x00~0xFF 1 byte (Inverter default 0x01) 1 byte 0x10 2 bytes Address MSB 0x0000-0xFFFF
Address LSB 2 bytes Number MSB 0x0001-0x007B Number LSB 8 Example 0x01 0x10 0x10 0x00 0x00 0x07 0x0E
0x58 0x42 0x34 0x30 0x34 0x30 0x30 0x30 0x30 0x30 0x30 0x30 0x30 0x30 0x57 0xEA 0x01 0x10 0x10 0x00
0x00 0x07 Field Name CRC Slave ID Number of Bytes Content Format 2 bytes CRC MSB 0x0000-0xFFFF CRC
LSB Slave Fault Response 0x00~0xFF 1 byte (Inverter default 0x01) Example 0x85 0x0B 0x01 Fault Code
Abnormal Code CRC 1 byte 1 byte 2 bytes CRC MSB CRC LSB 0x90 0x01 or 0x02 or 0x03 or 0x04
0x0000-0xFFFF 0x90 0x02 0xCD 0xC1

2.3 MODBUS Exception Codes Only some of the MODBUS exception responses are listed here. For more
details, please visit the MODBUS website: http://www.modbus.org. Code 01 02 03 04 Name ILLEGAL
FUNCTION ILLEGAL DATA ADDRESS ILLEGALVALUE DATASERVER DEVICE FAILURE MODBUS Exception Codes
Description The function code received in the query is not an allowable action for the server. This
may be because the function code is only applicable to newer devices, and was not implemented in the
nit selected. It could also indicate that the server s in the wrong state to process a request of
this type, for example because it is not configured and s being asked to return register values. The
data address received in the query is not an allowable address for the server. More specifically,
the combination of reference number and transfer length is invalid. A value contained in the query
data field is not an allowable value for server. This indicates a fault in the structure of the
remainder of a complex request, such as that the implied length is incorrect. Specialized use in
conjunction with programming commands. The server has accepted the request and is processing it, but
a long duration of time will be required to do so. This response is returned to prevent a timeout
error from occurring in the client.

3.1 Inverter Register Overview Function Code 0x03 0x04 0x06 0x10 Address Application 0x0000~0x01FF
0x0300~0x0400 0x0000~0x017F 0x01DD~0x02ED 0x0000~0x01FF 0x0000~0x007B 0x007C~0x008A 0x00A0~0x00A8
Read inverter model information and configuration parameters Read battery model information Read
inverter and battery real-time data Read inverter parallel system data Write inverter and battery
single configuration parameter Write inverter and battery multiply configuration parameters VPP mode
control (old interface) VPP mode control (new interface)
